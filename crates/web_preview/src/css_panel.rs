//! Live CSS editing panel for the picked element, with per-rule write-back to source.
//!
//! Reads the element's matched declarations with CDP `CSS.getMatchedStylesForNode` and shows *every*
//! matched rule (most specific first) plus the inline style, each in its own buffer-backed editor
//! (so the CSS language server attaches, matching `crates/inspector_ui/src/div_inspector.rs`). Edits
//! apply live with `CSS.setStyleTexts`; after each apply we re-fetch matched styles, which is the
//! simplest correct way to keep the editable ranges valid (CDP ranges shift as text changes).
//!
//! Write-back is routed per rule:
//! - **Deterministic:** rules in a plain external stylesheet (`regular` origin, not inline) whose
//!   `sourceURL` resolves to a project file are written straight into that file via `Buffer::edit`.
//! - **Agent fallback:** inline / scoped `<style>` / CSS-in-JS / Tailwind rules have no file-backed
//!   source range (under Vite they are *constructed* stylesheets, in-memory only — CDP exposes no
//!   reliable file mapping), so we hand the change to the agent (or copy a prompt to the clipboard).
//!
//! Editors require a `Window` to construct (`Editor::for_buffer`), so a fresh set is built each time
//! a node is picked, inside the window context the caller threads through.

use crate::cdp::CdpClient;
use anyhow::{Context as _, Result};
use collections::HashMap;
use editor::{Editor, EditorEvent};
use futures::FutureExt as _;
use gpui::{ClipboardItem, Entity, FocusHandle, Focusable, Task, WeakEntity, prelude::*};
use language::{Buffer, Point};
use parking_lot::Mutex;
use project::Project;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use ui::{Label, Tooltip, prelude::*};
use workspace::Toast;
use workspace::Workspace;
use workspace::notifications::NotificationId;

/// Shared map of `styleSheetId` → metadata, populated from `CSS.styleSheetAdded` events by the
/// session. Lets the panel decide whether an edited rule can be written back to a source file.
pub type StyleSheetRegistry = Arc<Mutex<HashMap<String, StyleSheetHeader>>>;

#[derive(Clone, Debug, Default)]
pub struct StyleSheetHeader {
    pub source_url: String,
    pub is_inline: bool,
    /// CDP `StyleSheetOrigin`: "regular" | "user-agent" | "inspector" | "injected".
    pub origin: String,
}

impl StyleSheetHeader {
    /// A `regular`, non-inline stylesheet with a URL is one we can try to map to a file.
    fn is_writable_source(&self) -> bool {
        self.origin == "regular" && !self.is_inline && !self.source_url.is_empty()
    }
}

/// Record a `CSS.styleSheetAdded` event's header into the registry.
pub fn record_style_sheet(registry: &StyleSheetRegistry, params: &Value) {
    let Some(header) = params.get("header") else {
        return;
    };
    let Some(id) = header.get("styleSheetId").and_then(Value::as_str) else {
        return;
    };
    registry.lock().insert(
        id.to_string(),
        StyleSheetHeader {
            source_url: header
                .get("sourceURL")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            is_inline: header
                .get("isInline")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            origin: header
                .get("origin")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
    );
}

/// The CDP `SourceRange` for a declaration block, plus its owning stylesheet and matched selector.
#[derive(Clone, Debug)]
struct EditTarget {
    style_sheet_id: String,
    start_line: u32,
    start_column: u32,
    end_line: u32,
    end_column: u32,
    /// The selector this rule matched (for the label and the agent prompt). `None` for inline.
    selector: Option<String>,
}

/// A loaded, raw matched rule (before editors are built).
struct LoadedRule {
    target: EditTarget,
    css_text: String,
}

/// Where a rule's edits can be persisted.
#[derive(Clone)]
enum WriteRoute {
    /// Plain external stylesheet mapped to this project file.
    Source(PathBuf),
    /// No file mapping (inline / scoped / constructed) — hand off to the agent.
    Agent,
}

impl WriteRoute {
    /// Short label shown next to the rule indicating where its edits will be saved.
    fn label(&self) -> SharedString {
        match self {
            WriteRoute::Source(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().to_string().into())
                .unwrap_or_else(|| "source".into()),
            WriteRoute::Agent => "scoped · agent".into(),
        }
    }
}

/// A rule shown in the panel: its target, write route, and its own editor.
struct RuleEntry {
    target: EditTarget,
    route: WriteRoute,
    selector: SharedString,
    editor: Entity<Editor>,
    _subscription: gpui::Subscription,
}

pub struct CssPanel {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    style_sheets: StyleSheetRegistry,
    focus_handle: FocusHandle,
    cdp: Option<CdpClient>,
    node_id: Option<i64>,
    rules: Vec<RuleEntry>,
    status: Status,
    /// Guards against re-applying while we programmatically reset a buffer's text.
    suppress_apply: bool,
    _load_task: Option<Task<()>>,
    _apply_task: Option<Task<()>>,
    _write_task: Option<Task<()>>,
}

enum Status {
    Empty,
    Loading,
    Editing,
    Error(SharedString),
}

impl CssPanel {
    pub fn new(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        style_sheets: StyleSheetRegistry,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            project,
            workspace,
            style_sheets,
            focus_handle: cx.focus_handle(),
            cdp: None,
            node_id: None,
            rules: Vec::new(),
            status: Status::Empty,
            suppress_apply: false,
            _load_task: None,
            _apply_task: None,
            _write_task: None,
        }
    }

    /// Load all matched rules for a newly picked node and build an editor for each.
    pub fn load_for_node(
        &mut self,
        cdp: CdpClient,
        node_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cdp = Some(cdp.clone());
        self.node_id = Some(node_id);
        self.status = Status::Loading;
        self.rules.clear();
        cx.notify();

        let task = cx.spawn_in(window, async move |this, cx| {
            let result = Self::fetch_matched_rules(&cdp, node_id).await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(loaded) => this.build_rule_editors(loaded, window, cx),
                Err(error) => {
                    this.status = Status::Error(format!("{error:#}").into());
                    cx.notify();
                }
            })
            .ok();
        });
        self._load_task = Some(task);
    }

    /// Build one editor per loaded rule and resolve each rule's write route.
    fn build_rule_editors(
        &mut self,
        loaded: Vec<LoadedRule>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let css_language = self
            .project
            .read(cx)
            .languages()
            .language_for_name("CSS")
            .now_or_never()
            .and_then(Result::ok);

        let mut rules = Vec::with_capacity(loaded.len());
        for (index, rule) in loaded.into_iter().enumerate() {
            let buffer = cx.new(|cx| {
                let mut buffer = Buffer::local(rule.css_text.clone(), cx);
                if let Some(language) = css_language.clone() {
                    buffer.set_language(Some(language), cx);
                }
                buffer
            });
            let editor =
                cx.new(|cx| Editor::for_buffer(buffer, Some(self.project.clone()), window, cx));

            let subscription =
                cx.subscribe(&editor, move |this, editor, event: &EditorEvent, cx| {
                    if matches!(event, EditorEvent::BufferEdited) && !this.suppress_apply {
                        let text = editor.read(cx).text(cx);
                        this.apply_live(index, text, cx);
                    }
                });

            let route = self.route_for(&rule.target, cx);
            let selector = rule
                .target
                .selector
                .clone()
                .unwrap_or_else(|| "element.style (inline)".to_string());

            rules.push(RuleEntry {
                target: rule.target,
                route,
                selector: selector.into(),
                editor,
                _subscription: subscription,
            });
        }

        self.rules = rules;
        self.status = if self.rules.is_empty() {
            Status::Empty
        } else {
            Status::Editing
        };
        cx.notify();
    }

    /// Decide where a rule's edits can be persisted, from the stylesheet registry.
    fn route_for(&self, target: &EditTarget, cx: &App) -> WriteRoute {
        let header = self
            .style_sheets
            .lock()
            .get(&target.style_sheet_id)
            .cloned();
        match header {
            Some(header) if header.is_writable_source() => {
                match source_url_to_abs_path(&self.project, &header.source_url, cx) {
                    Some(path) => WriteRoute::Source(path),
                    None => WriteRoute::Agent,
                }
            }
            _ => WriteRoute::Agent,
        }
    }

    /// Fetch every matched rule (most specific first) plus the inline style.
    async fn fetch_matched_rules(cdp: &CdpClient, node_id: i64) -> Result<Vec<LoadedRule>> {
        let matched = cdp
            .send("CSS.getMatchedStylesForNode", json!({ "nodeId": node_id }))
            .await
            .context("CSS.getMatchedStylesForNode")?;

        let mut rules = Vec::new();

        // Inline style first (it wins the cascade), if the element has one.
        if let Some(inline) = matched.get("inlineStyle") {
            if let Some(target) = parse_edit_target(inline) {
                rules.push(LoadedRule {
                    css_text: declaration_text(inline),
                    target,
                });
            }
        }

        // Matched rules, most specific first (CDP returns least→most specific).
        if let Some(matched_rules) = matched.get("matchedCSSRules").and_then(Value::as_array) {
            for rule_match in matched_rules.iter().rev() {
                let Some(rule) = rule_match.get("rule") else {
                    continue;
                };
                let Some(style) = rule.get("style") else {
                    continue;
                };
                let Some(mut target) = parse_edit_target(style) else {
                    continue;
                };
                target.selector = rule
                    .get("selectorList")
                    .and_then(|list| list.get("text"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                rules.push(LoadedRule {
                    css_text: declaration_text(style),
                    target,
                });
            }
        }

        Ok(rules)
    }

    /// Push rule `index`'s current declaration text to the page live via CDP.
    fn apply_live(&mut self, index: usize, css_text: String, cx: &mut Context<Self>) {
        let (Some(cdp), Some(rule)) = (self.cdp.clone(), self.rules.get(index)) else {
            return;
        };
        let target = rule.target.clone();
        let node_id = self.node_id;

        let task = cx.spawn(async move |this, cx| {
            match cdp
                .send("CSS.setStyleTexts", set_style_edit(&target, &css_text))
                .await
            {
                Ok(_) => {
                    // Re-fetch to keep this rule's edit range valid after the stylesheet changed.
                    if let Some(node_id) = node_id {
                        if let Ok(loaded) = Self::fetch_matched_rules(&cdp, node_id).await {
                            this.update(cx, |this, _| {
                                if let Some(fresh) = loaded.get(index) {
                                    if let Some(entry) = this.rules.get_mut(index) {
                                        entry.target = fresh.target.clone();
                                    }
                                }
                            })
                            .ok();
                        }
                    }
                }
                Err(error) => log::warn!("CSS.setStyleTexts failed: {error:#}"),
            }
        });
        self._apply_task = Some(task);
    }

    /// Persist rule `index` to source: deterministically when its route is a file, else via agent.
    fn write_rule_to_source(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rule) = self.rules.get(index) else {
            return;
        };
        let css_text = rule.editor.read(cx).text(cx);
        let target = rule.target.clone();

        match rule.route.clone() {
            WriteRoute::Source(abs_path) => {
                self.write_deterministic(abs_path, &css_text, &target, window, cx)
            }
            WriteRoute::Agent => self.write_via_agent(&css_text, &target, window, cx),
        }
    }

    /// Deterministic write-back: open the source file and replace the rule's declaration range.
    fn write_deterministic(
        &mut self,
        abs_path: PathBuf,
        css_text: &str,
        target: &EditTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !abs_path.exists() {
            // The file moved/disappeared since the route was resolved; degrade to the agent path.
            self.write_via_agent(css_text, target, window, cx);
            return;
        }
        let project = self.project.clone();
        let css_text = css_text.to_string();
        let range_start = Point::new(target.start_line, target.start_column);
        let range_end = Point::new(target.end_line, target.end_column);

        let task = cx.spawn(async move |this, cx| {
            let result: Result<()> = async {
                let buffer = project
                    .update(cx, |project, cx| project.open_local_buffer(&abs_path, cx))
                    .await?;
                buffer.update(cx, |buffer, cx| {
                    let start = buffer.clip_point(range_start, language::Bias::Left);
                    let end = buffer.clip_point(range_end, language::Bias::Right);
                    buffer.edit([(start..end, css_text.clone())], None, cx);
                });
                project
                    .update(cx, |project, cx| project.save_buffer(buffer, cx))
                    .await?;
                Ok(())
            }
            .await;

            if let Err(error) = result {
                log::error!("web preview CSS write-back failed: {error:#}");
                this.update(cx, |this, cx| {
                    this.status =
                        Status::Error(format!("Write to source failed: {error:#}").into());
                    cx.notify();
                })
                .ok();
            }
        });
        self._write_task = Some(task);
    }

    /// Agent fallback for rules with no deterministic source range. Best-effort opens the agent
    /// panel pre-filled with a prompt; if unavailable, copies the prompt to the clipboard.
    fn write_via_agent(
        &mut self,
        css_text: &str,
        target: &EditTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prompt = agent_prompt(target, css_text);
        let workspace = self.workspace.clone();

        let opened = workspace
            .update(cx, |workspace, cx| {
                open_agent_with_prompt(workspace, &prompt, window, cx)
            })
            .unwrap_or(false);

        if opened {
            return;
        }

        cx.write_to_clipboard(ClipboardItem::new_string(prompt));
        workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(
                        NotificationId::unique::<CssWriteBackNotice>(),
                        "Couldn't write these styles to source automatically. An agent prompt was copied to your clipboard.",
                    ),
                    cx,
                );
            })
            .ok();
    }
}

struct CssWriteBackNotice;

/// Extract a CDP `CSSStyle`'s declaration text.
fn declaration_text(style: &Value) -> String {
    style
        .get("cssText")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Build the `CSS.setStyleTexts` edit payload for a target + new declaration text.
fn set_style_edit(target: &EditTarget, css_text: &str) -> Value {
    json!({
        "edits": [{
            "styleSheetId": target.style_sheet_id,
            "range": {
                "startLine": target.start_line,
                "startColumn": target.start_column,
                "endLine": target.end_line,
                "endColumn": target.end_column,
            },
            "text": css_text,
        }]
    })
}

/// Parse a CDP `CSSStyle` object into the range we edit. Returns `None` if it has no editable range
/// (e.g. user-agent styles).
fn parse_edit_target(style: &Value) -> Option<EditTarget> {
    let style_sheet_id = style.get("styleSheetId")?.as_str()?.to_string();
    let range = style.get("range")?;
    Some(EditTarget {
        style_sheet_id,
        start_line: range.get("startLine")?.as_u64()? as u32,
        start_column: range.get("startColumn")?.as_u64()? as u32,
        end_line: range.get("endLine")?.as_u64()? as u32,
        end_column: range.get("endColumn")?.as_u64()? as u32,
        selector: None,
    })
}

/// Map a stylesheet `sourceURL` (e.g. `http://localhost:5173/src/app.css`) to an absolute path in a
/// visible worktree, by matching the URL's path tail against worktree files.
fn source_url_to_abs_path(
    project: &Entity<Project>,
    source_url: &str,
    cx: &App,
) -> Option<PathBuf> {
    let path_part = source_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(source_url);
    let path_part = path_part.split(['?', '#']).next().unwrap_or(path_part);
    // Drop the leading host segment if present (`localhost:5173/src/app.css` → `src/app.css`).
    let relative = path_part
        .split_once('/')
        .map(|(_, rest)| rest)
        .unwrap_or(path_part)
        .trim_start_matches('/');

    let relative = PathBuf::from(relative);
    for worktree in project.read(cx).visible_worktrees(cx) {
        let candidate = worktree.read(cx).abs_path().join(&relative);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Compose the prompt sent to the agent for non-deterministic write-back.
fn agent_prompt(target: &EditTarget, css_text: &str) -> String {
    let selector = target
        .selector
        .as_deref()
        .map(|s| format!(" for the rule `{s}`"))
        .unwrap_or_default();
    format!(
        "Update the styles in the source code{selector} to match these declarations, then keep \
         them in sync with the component's existing styling approach (scoped style block, \
         CSS-in-JS, or utility classes as appropriate):\n\n{css_text}"
    )
}

/// Best-effort: focus the agent panel and pre-fill its composer with `prompt`. Returns whether the
/// panel was found (and thus the prompt inserted).
fn open_agent_with_prompt(
    workspace: &mut Workspace,
    prompt: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    let Some(panel) = workspace.panel::<agent_ui::AgentPanel>(cx) else {
        return false;
    };
    workspace.focus_panel::<agent_ui::AgentPanel>(window, cx);
    panel.update(cx, |panel, cx| {
        if let Some(conversation_view) = panel.active_conversation_view() {
            conversation_view.update(cx, |conversation_view, cx| {
                if let Some(thread_view) = conversation_view.active_thread() {
                    thread_view.update(cx, |thread_view, cx| {
                        thread_view.message_editor.update(cx, |editor, cx| {
                            editor.insert_text(prompt, window, cx);
                        });
                    });
                }
            });
        }
    });
    true
}

impl Focusable for CssPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CssPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();

        let body = match &self.status {
            Status::Empty => div()
                .p_2()
                .child(Label::new("Pick an element to edit its styles"))
                .into_any_element(),
            Status::Loading => div()
                .p_2()
                .child(Label::new("Loading styles…"))
                .into_any_element(),
            Status::Error(message) => div()
                .p_2()
                .child(Label::new(message.clone()).color(Color::Error))
                .into_any_element(),
            Status::Editing => {
                let rows = self.rules.iter().enumerate().map(|(index, rule)| {
                    let route_label = rule.route.label();
                    v_flex()
                        .gap_0p5()
                        .py_1()
                        .border_b_1()
                        .border_color(colors.border_variant)
                        .child(
                            h_flex()
                                .justify_between()
                                .px_2()
                                .child(
                                    Label::new(rule.selector.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .child(
                                            Label::new(route_label)
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        )
                                        .child(
                                            ui::Button::new(("write", index), "Write")
                                                .tooltip(Tooltip::text(
                                                    "Save this rule to source (or hand off to the agent)",
                                                ))
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.write_rule_to_source(index, window, cx);
                                                    },
                                                )),
                                        ),
                                ),
                        )
                        .child(div().px_2().child(rule.editor.clone()))
                });
                v_flex()
                    .id("web-preview-css-rules")
                    .overflow_y_scroll()
                    .children(rows)
                    .into_any_element()
            }
        };

        v_flex()
            .size_full()
            .bg(colors.panel_background)
            .child(
                div()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(Label::new("Styles")),
            )
            .child(div().flex_1().child(body))
    }
}

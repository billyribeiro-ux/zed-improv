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
use editor::{Editor, EditorEvent, EditorMode, MultiBuffer};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{ClipboardItem, Entity, FocusHandle, Focusable, Task, WeakEntity, prelude::*};
use language::{Bias, Buffer, PointUtf16, Unclipped};
use parking_lot::Mutex;
use project::Project;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use ui::{Label, Tooltip, prelude::*};
use workspace::Toast;
use workspace::Workspace;
use workspace::notifications::NotificationId;

/// How long to wait for typing to settle before applying a live CSS edit. Coalesces a burst of
/// keystrokes into a single `CSS.setStyleTexts` + re-fetch.
const APPLY_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);

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
///
/// Note: CDP `SourceRange` columns are **UTF-16 code-unit** offsets, not byte offsets. They are
/// converted to byte offsets via `PointUtf16` before any `Buffer::edit` (see `write_deterministic`).
#[derive(Clone, Debug)]
struct EditTarget {
    style_sheet_id: String,
    start_line: u32,
    start_column: u32,
    end_line: u32,
    end_column: u32,
    /// The selector this rule matched (for the label and the agent prompt). `None` for inline.
    selector: Option<String>,
    /// The declaration text CDP reported for this range at load. Used to verify the on-disk span
    /// still matches before a deterministic write, so we never clobber drifted source.
    original_css: String,
}

/// Outcome of a deterministic write attempt.
enum WriteOutcome {
    /// The edit was applied (caller saves the buffer).
    Written,
    /// The on-disk text diverged from what CDP reported — not written, to avoid corruption.
    Drifted,
    /// The reported range didn't resolve to a valid span in the file.
    RangeInvalid,
}

/// Normalize CSS declaration text for the drift comparison: collapse all runs of ASCII whitespace
/// to a single space and trim. Tolerates trivial formatting differences (indentation, trailing
/// semicolons spacing) between CDP's report and on-disk text without tolerating real content changes.
fn normalize_css(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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
    /// Live-apply target, refreshed after every `CSS.setStyleTexts` (browser-side ranges shift as
    /// text changes).
    target: EditTarget,
    /// The target as loaded at pick time, never refreshed. Deterministic write-back compares the
    /// ON-DISK file against what the browser had loaded *originally*; the refreshed `target`
    /// reflects post-live-edit browser state and would always trip the drift guard.
    loaded_target: EditTarget,
    route: WriteRoute,
    selector: SharedString,
    editor: Entity<Editor>,
    /// Failure of the most recent Write attempt for THIS rule, shown inline. Kept per-rule so one
    /// failed write doesn't blank the whole panel.
    write_error: Option<SharedString>,
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
    /// A transient message shown when a *settled* live edit fails to apply (read-only sheet,
    /// detached node) — distinct from the expected mid-token parse failures, which are not surfaced.
    apply_error: Option<SharedString>,
    /// Queue feeding the long-lived apply worker. Edits are enqueued per keystroke; the worker
    /// debounces and serializes them, so an in-flight `setStyleTexts` + range re-fetch is never
    /// cancelled mid-flight by the next keystroke (which left ranges stale and corrupted later
    /// edits when the apply task itself was replaced per keystroke).
    apply_queue: Option<mpsc::UnboundedSender<(usize, String)>>,
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
            apply_error: None,
            apply_queue: None,
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
        self.apply_error = None;
        cx.notify();
        self.spawn_apply_worker(cdp.clone(), node_id, cx);

        let languages = self.project.read(cx).languages().clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            // Await the CSS language so the first pick gets syntax highlighting / LSP, rather than
            // intermittently falling back to plain text on a cold registry (`now_or_never`).
            let css_language = languages.language_for_name("CSS").await.ok();
            let result = Self::fetch_matched_rules(&cdp, node_id).await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(loaded) => this.build_rule_editors(loaded, css_language, window, cx),
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
        css_language: Option<Arc<language::Language>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut rules = Vec::with_capacity(loaded.len());
        for (index, rule) in loaded.into_iter().enumerate() {
            let buffer = cx.new(|cx| {
                let mut buffer = Buffer::local(rule.css_text.clone(), cx);
                if let Some(language) = css_language.clone() {
                    buffer.set_language(Some(language), cx);
                }
                buffer
            });
            // Auto-height (content-sized): a full-mode editor requests `relative(1.)` height,
            // which collapses to ZERO inside this auto-height rule list — the panel rendered
            // selectors and buttons but no visible CSS at all.
            let multibuffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
            let editor = cx.new(|cx| {
                Editor::new(
                    EditorMode::AutoHeight {
                        min_lines: 1,
                        max_lines: Some(12),
                    },
                    multibuffer,
                    Some(self.project.clone()),
                    window,
                    cx,
                )
            });

            let subscription =
                cx.subscribe(&editor, move |this, editor, event: &EditorEvent, cx| {
                    if matches!(event, EditorEvent::BufferEdited) {
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
                loaded_target: rule.target.clone(),
                target: rule.target,
                route,
                selector: selector.into(),
                editor,
                write_error: None,
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
                match crate::path_resolve::resolve(&self.project, &header.source_url, cx) {
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

    /// Enqueue a live-apply of rule `index`. The worker debounces and serializes the queue.
    fn apply_live(&mut self, index: usize, css_text: String, _cx: &mut Context<Self>) {
        if let Some(queue) = &self.apply_queue {
            queue.unbounded_send((index, css_text)).ok();
        }
    }

    /// The long-lived apply worker for the current pick. Debounces keystrokes, then sends ONE
    /// `CSS.setStyleTexts` and re-fetches all matched ranges. Running edits through a single
    /// serialized worker (instead of one cancellable task per keystroke) guarantees an in-flight
    /// apply + range re-fetch is never cancelled midway — a cancelled re-fetch left `target`
    /// ranges stale, and the next edit then replaced the wrong span in the browser stylesheet.
    /// The rule's target is read AFTER the debounce settles, never captured at keystroke time.
    fn spawn_apply_worker(&mut self, cdp: CdpClient, node_id: i64, cx: &mut Context<Self>) {
        let (queue, mut pending_edits) = mpsc::unbounded::<(usize, String)>();
        self.apply_queue = Some(queue);
        let executor = cx.background_executor().clone();

        self._apply_task = Some(cx.spawn(async move |this, cx| {
            while let Some(mut edit) = pending_edits.next().await {
                // Debounce: keep absorbing newer edits until a full quiet period passes.
                loop {
                    executor.timer(APPLY_DEBOUNCE).await;
                    let mut newest = None;
                    while let Ok(newer) = pending_edits.try_recv() {
                        newest = Some(newer);
                    }
                    match newest {
                        Some(newer) => edit = newer,
                        None => break,
                    }
                }
                let (index, css_text) = edit;
                let Ok(Some(target)) = this.read_with(cx, |this, _| {
                    this.rules.get(index).map(|rule| rule.target.clone())
                }) else {
                    continue;
                };

                match cdp
                    .send("CSS.setStyleTexts", set_style_edit(&target, &css_text))
                    .await
                {
                    Ok(_) => {
                        // Re-fetch and refresh the ranges of *every* rule: editing one rule shifts
                        // the offsets of later rules in the same stylesheet, so only updating
                        // `index` would leave siblings stale.
                        if let Ok(loaded) = Self::fetch_matched_rules(&cdp, node_id).await {
                            this.update(cx, |this, cx| {
                                this.refresh_targets(&loaded);
                                this.clear_apply_error(cx);
                            })
                            .ok();
                        }
                    }
                    Err(error) => {
                        // Settled text that still fails is a real problem (read-only sheet,
                        // detached node), not a mid-token typo — surface it.
                        log::warn!("CSS.setStyleTexts failed: {error:#}");
                        this.update(cx, |this, cx| {
                            this.apply_error =
                                Some("Couldn't apply this change to the page.".into());
                            cx.notify();
                        })
                        .ok();
                    }
                }
            }
        }));
    }

    /// Refresh every rule entry's live `target` from a fresh fetch, matched by stable identity
    /// (`styleSheetId` + selector) so positional drift can't mis-assign ranges. Duplicate
    /// selectors in one sheet (e.g. the same rule inside and outside an `@media` block) are paired
    /// by occurrence order — both lists come from the same fetch ordering — so the Nth duplicate
    /// entry gets the Nth duplicate's range, not the first one's. `loaded_target` (the write-back
    /// snapshot) is deliberately never refreshed.
    fn refresh_targets(&mut self, loaded: &[LoadedRule]) {
        let mut claimed = vec![false; loaded.len()];
        for entry in &mut self.rules {
            let fresh = loaded.iter().enumerate().find(|(index, candidate)| {
                !claimed[*index]
                    && candidate.target.style_sheet_id == entry.target.style_sheet_id
                    && candidate.target.selector == entry.target.selector
            });
            if let Some((index, fresh)) = fresh {
                claimed[index] = true;
                entry.target = fresh.target.clone();
            }
        }
    }

    fn clear_apply_error(&mut self, cx: &mut Context<Self>) {
        if self.apply_error.take().is_some() {
            cx.notify();
        }
    }

    /// Show a short transient toast (e.g. confirming a successful write to source).
    fn notify(&self, message: String, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<CssWriteBackNotice>(), message),
                    cx,
                );
            })
            .ok();
    }

    /// Persist rule `index` to source: deterministically when its route is a file, else via agent.
    fn write_rule_to_source(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rule) = self.rules.get(index) else {
            return;
        };
        let css_text = rule.editor.read(cx).text(cx);
        // Write-back uses the LOAD-TIME target: the drift guard compares the on-disk file (which
        // live edits never touch) against what the browser had loaded at pick time. The refreshed
        // live `target` holds post-edit browser text/ranges and would always report drift.
        let target = rule.loaded_target.clone();

        match rule.route.clone() {
            WriteRoute::Source(abs_path) => {
                self.write_deterministic(index, abs_path, &css_text, &target, window, cx)
            }
            WriteRoute::Agent => self.write_via_agent(&css_text, &target, window, cx),
        }
    }

    /// Record a Write failure on the affected rule only, leaving every editor in place. (Replacing
    /// the whole body with an error label unmounted all editors mid-edit.)
    fn set_write_error(&mut self, index: usize, message: SharedString, cx: &mut Context<Self>) {
        if let Some(rule) = self.rules.get_mut(index) {
            rule.write_error = Some(message);
            cx.notify();
        }
    }

    /// Deterministic write-back: open the source file and replace the rule's declaration range.
    ///
    /// Two safety properties protect the user's source from silent corruption:
    /// 1. **Encoding:** CDP `SourceRange` columns are UTF-16 code units; they are converted to byte
    ///    offsets via `PointUtf16` before editing, so lines with non-ASCII content edit correctly.
    /// 2. **Drift guard:** the on-disk text at the reported range is compared against the declaration
    ///    CDP originally reported (`target.original_css`). On mismatch — the browser's loaded CSS has
    ///    diverged from disk (HMR-stale, transformed output, or the user edited it in Zed) — we do
    ///    NOT edit; we surface an error and leave the file untouched.
    fn write_deterministic(
        &mut self,
        index: usize,
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
        let original_css = target.original_css.clone();
        // CDP columns are UTF-16 code units.
        let range_start = PointUtf16::new(target.start_line, target.start_column);
        let range_end = PointUtf16::new(target.end_line, target.end_column);

        let task = cx.spawn(async move |this, cx| {
            let outcome: Result<WriteOutcome> = async {
                let buffer = project
                    .update(cx, |project, cx| project.open_local_buffer(&abs_path, cx))
                    .await?;
                let outcome = buffer.update(cx, |buffer, cx| {
                    let snapshot = buffer.snapshot();
                    // UTF-16 (line, col) -> clamped PointUtf16 -> byte offset.
                    let start = snapshot.point_utf16_to_offset(
                        snapshot.clip_point_utf16(Unclipped(range_start), Bias::Left),
                    );
                    let end = snapshot.point_utf16_to_offset(
                        snapshot.clip_point_utf16(Unclipped(range_end), Bias::Right),
                    );
                    if start > end || end > buffer.len() {
                        return WriteOutcome::RangeInvalid;
                    }
                    // Drift guard: the on-disk span must still match what CDP reported.
                    let on_disk: String = snapshot.text_for_range(start..end).collect();
                    if normalize_css(&on_disk) != normalize_css(&original_css) {
                        return WriteOutcome::Drifted;
                    }
                    buffer.edit([(start..end, css_text.clone())], None, cx);
                    WriteOutcome::Written
                });
                if matches!(outcome, WriteOutcome::Written) {
                    project
                        .update(cx, |project, cx| project.save_buffer(buffer, cx))
                        .await?;
                }
                Ok(outcome)
            }
            .await;

            this.update(cx, |this, cx| match outcome {
                Ok(WriteOutcome::Written) => {
                    this.notify("Saved to source".to_string(), cx);
                    this.clear_apply_error(cx);
                    if let Some(rule) = this.rules.get_mut(index) {
                        rule.write_error = None;
                        cx.notify();
                    }
                }
                Ok(WriteOutcome::Drifted) => {
                    this.set_write_error(
                        index,
                        "Source has changed since this rule was loaded — not written, to avoid \
                         overwriting unrelated code. Re-pick the element, or use the agent."
                            .into(),
                        cx,
                    );
                }
                Ok(WriteOutcome::RangeInvalid) => {
                    this.set_write_error(
                        index,
                        "Couldn't locate this rule's range in the source file.".into(),
                        cx,
                    );
                }
                Err(error) => {
                    log::error!("web preview CSS write-back failed: {error:#}");
                    this.set_write_error(
                        index,
                        format!("Write to source failed: {error:#}").into(),
                        cx,
                    );
                }
            })
            .ok();
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
        original_css: declaration_text(style),
    })
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
                    let write_error = rule.write_error.clone();
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
                        .when_some(write_error, |this, message| {
                            this.child(
                                div().px_2().child(
                                    Label::new(message)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Error),
                                ),
                            )
                        })
                });
                v_flex()
                    .id("web-preview-css-rules")
                    // `size_full` is what gives the scroll container a bounded height; without it
                    // the element is content-sized and the scroll range is zero (list unscrollable).
                    .size_full()
                    .overflow_y_scroll()
                    .children(rows)
                    .into_any_element()
            }
        };

        let apply_error = self.apply_error.clone();
        v_flex()
            .size_full()
            .bg(colors.panel_background)
            .child(
                h_flex()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(Label::new("Styles"))
                    .when_some(apply_error, |this, message| {
                        this.child(
                            Label::new(message)
                                .size(LabelSize::Small)
                                .color(Color::Error),
                        )
                    }),
            )
            .child(div().flex_1().child(body))
    }
}

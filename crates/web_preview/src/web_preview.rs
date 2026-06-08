//! In-editor web app preview with deterministic click-to-source and live CSS editing.
//!
//! Unlike Cursor — whose "Select element" forwards a node to an AI agent that *infers* the source
//! file — this previews the user's running web app inside Zed and resolves a clicked element to its
//! exact `file:line:column` deterministically, using the framework's dev-mode source metadata
//! (Svelte's `__svelte_meta` in v1). The page is rendered inside the panel via a CDP screencast.
//!
//! Architecture: an external Chrome is launched with remote debugging ([`chrome`]), driven over the
//! Chrome DevTools Protocol ([`cdp`]). The page is streamed into the panel ([`screencast`]); clicks
//! are forwarded back to the page; pick mode resolves elements to source ([`source_map`]); and the
//! CSS panel reads/edits live styles ([`css_panel`]).

mod cdp;
mod chrome;
mod css_panel;
mod dev_server;
mod screencast;
mod source_map;
mod web_preview_settings;

use anyhow::{Context as _, Result};
use cdp::CdpClient;
use css_panel::CssPanel;
use futures::StreamExt as _;
use gpui::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Bounds, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, MouseButton, MouseDownEvent, Pixels, Task, WeakEntity, Window, canvas,
    div, img, prelude::*, px,
};
use language::Point as TextPoint;
use screencast::DecodedFrame;
use serde_json::{Value, json};
use settings::Settings as _;
use source_map::{Framework, Resolution, SourceLocation};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use ui::{Icon, IconButton, IconName, IconSize, Label, Tooltip, prelude::*};
use util::ResultExt as _;
use web_preview_settings::WebPreviewSettings;
use workspace::item::{Item, ItemEvent};
use workspace::{AppState, OpenOptions, Workspace};

use project::Project;

gpui::actions!(
    web_preview,
    [
        /// Open the web app preview panel.
        OpenWebPreview,
        /// Toggle element pick mode in the web preview (click an element to open its source).
        ToggleWebPickMode,
    ]
);

/// User-facing product name for the panel.
const PRODUCT_NAME: &str = "Looking Glass";

pub fn init(app_state: Arc<AppState>, cx: &mut App) {
    WebPreviewSettings::register(cx);

    cx.observe_new(move |workspace: &mut Workspace, _window, cx| {
        let app_state = app_state.clone();
        let project = workspace.project().clone();
        let weak_workspace = cx.entity().downgrade();

        workspace.register_action(move |workspace, _: &OpenWebPreview, window, cx| {
            let view = cx.new(|cx| {
                WebPreviewView::new(
                    weak_workspace.clone(),
                    project.clone(),
                    app_state.clone(),
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        });
    })
    .detach();
}

/// Connection lifecycle of the preview session.
enum SessionState {
    Connecting,
    Connected(CdpClient),
    Failed(SharedString),
}

pub struct WebPreviewView {
    focus_handle: FocusHandle,
    window_handle: AnyWindowHandle,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    app_state: Arc<AppState>,
    state: SessionState,
    framework: Framework,
    picking: bool,
    /// Whether the in-panel shortcuts/help overlay is showing.
    show_help: bool,
    latest_frame: Option<DecodedFrame>,
    /// Bounds of the rendered preview image, captured each frame for click→page coordinate mapping.
    image_bounds: Option<Bounds<Pixels>>,
    css_panel: Entity<CssPanel>,
    /// Shared `styleSheetId` → header map, populated from `CSS.styleSheetAdded`; used by the CSS
    /// panel to decide whether an edited rule can be written back to source.
    style_sheets: css_panel::StyleSheetRegistry,
    /// The launched Chrome process; kept alive (and killed on drop) for the view's lifetime.
    _chrome: Option<chrome::ChromeProcess>,
    _tasks: Vec<Task<()>>,
}

impl WebPreviewView {
    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        app_state: Arc<AppState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let style_sheets: css_panel::StyleSheetRegistry = Default::default();
        let css_panel = cx.new(|cx| {
            CssPanel::new(
                project.clone(),
                workspace.clone(),
                style_sheets.clone(),
                window,
                cx,
            )
        });
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            window_handle: window.window_handle(),
            workspace,
            project,
            app_state,
            state: SessionState::Connecting,
            framework: Framework::Unknown,
            picking: false,
            show_help: false,
            latest_frame: None,
            image_bounds: None,
            css_panel,
            style_sheets,
            _chrome: None,
            _tasks: Vec::new(),
        };
        this.connect(cx);
        this
    }

    /// Establish the full session: wait for the dev server, launch Chrome, connect CDP, enable the
    /// domains we need, detect the framework, and start streaming frames into the panel.
    fn connect(&mut self, cx: &mut Context<Self>) {
        let settings = WebPreviewSettings::get_global(cx).clone();
        let http_client: Arc<dyn http_client::HttpClient> = self.app_state.client.http_client();
        let executor = cx.background_executor().clone();

        let task = cx.spawn(async move |this, cx| {
            let result = Self::establish_session(settings, http_client, executor, &this, cx).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(cdp) => this.state = SessionState::Connected(cdp),
                    Err(error) => {
                        log::error!("web preview session failed: {error:#}");
                        this.state = SessionState::Failed(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        });
        self._tasks.push(task);
    }

    async fn establish_session(
        settings: WebPreviewSettings,
        http_client: Arc<dyn http_client::HttpClient>,
        executor: gpui::BackgroundExecutor,
        this: &WeakEntity<Self>,
        cx: &mut gpui::AsyncApp,
    ) -> Result<CdpClient> {
        dev_server::wait_until_ready(
            &settings.url,
            http_client.clone(),
            executor.clone(),
            Duration::from_secs(30),
        )
        .await
        .context("waiting for dev server")?;

        let chrome_path = chrome::locate_chrome(settings.chrome_path.as_deref())?;
        let user_data_dir = std::env::temp_dir().join("zed-web-preview-chrome");
        let process = chrome::launch(
            chrome_path,
            &settings.url,
            settings.remote_debugging_port,
            user_data_dir,
            http_client,
            executor,
        )
        .await
        .context("launching chrome")?;

        let ws_url = process.ws_url.clone();
        let cdp = CdpClient::connect(ws_url, cx)
            .await
            .context("connecting CDP")?;

        // Keep the Chrome process alive for the lifetime of the view.
        this.update(cx, |this, _| this._chrome = Some(process)).ok();

        // Subscribe to stylesheet headers BEFORE enabling the CSS domain, since `CSS.enable` replays
        // `CSS.styleSheetAdded` for existing sheets — subscribing after would miss them.
        let mut style_sheets_added = cdp.subscribe("CSS.styleSheetAdded");
        if let Ok(registry) = this.read_with(cx, |this, _| this.style_sheets.clone()) {
            let task = cx.background_spawn(async move {
                while let Some(params) = style_sheets_added.next().await {
                    css_panel::record_style_sheet(&registry, &params);
                }
            });
            this.update(cx, |this, _| this._tasks.push(task)).ok();
        }

        for domain in ["Page", "DOM", "CSS", "Overlay", "Runtime"] {
            cdp.send(&format!("{domain}.enable"), Value::Null)
                .await
                .with_context(|| format!("enabling {domain}"))?;
        }

        let framework = source_map::detect_framework(&cdp).await;
        this.update(cx, |this, _| this.framework = framework).ok();

        screencast::start(&cdp)
            .await
            .context("starting screencast")?;
        this.update(cx, |this, cx| {
            this.spawn_event_loops(cdp.clone(), cx);
        })
        .ok();

        Ok(cdp)
    }

    /// Subscribe to the CDP events we care about and route them back onto the view. Runs in the
    /// view's own context so the spawned tasks can update `self`.
    fn spawn_event_loops(&mut self, cdp: CdpClient, cx: &mut Context<Self>) {
        // (Stylesheet headers are subscribed earlier, before `CSS.enable`, in `establish_session`.)

        // Screencast frames: decode on a background thread, apply on the foreground.
        let mut frames = cdp.subscribe("Page.screencastFrame");
        let frame_cdp = cdp.clone();
        let frame_task = cx.spawn(async move |this, cx| {
            while let Some(params) = frames.next().await {
                if let Some(session_id) = screencast::session_id(&params) {
                    screencast::ack(&frame_cdp, session_id).await.log_err();
                }
                let decoded = cx
                    .background_spawn(async move { screencast::decode_frame(&params) })
                    .await;
                match decoded {
                    Ok(frame) => {
                        let dropped = this.update(cx, |this, cx| {
                            let previous = this.latest_frame.replace(frame);
                            cx.notify();
                            previous
                        });
                        if dropped.is_err() {
                            break;
                        }
                    }
                    Err(error) => log::warn!("screencast frame decode failed: {error:#}"),
                }
            }
        });
        self._tasks.push(frame_task);

        // Pick-mode element selection. This is the last consumer of `cdp`, so move it in.
        let mut picks = cdp.subscribe("Overlay.inspectNodeRequested");
        let pick_cdp = cdp;
        let pick_task = cx.spawn(async move |this, cx| {
            while let Some(params) = picks.next().await {
                let backend_node_id = params
                    .get("backendNodeId")
                    .and_then(Value::as_i64)
                    .unwrap_or_default();
                WebPreviewView::on_node_picked(&pick_cdp, &this, backend_node_id, cx)
                    .await
                    .log_err();
            }
        });
        self._tasks.push(pick_task);
    }

    async fn on_node_picked(
        cdp: &CdpClient,
        this: &WeakEntity<Self>,
        backend_node_id: i64,
        cx: &mut gpui::AsyncApp,
    ) -> Result<()> {
        // Resolve the picked node to a DOM nodeId and a JS object handle.
        let push = cdp
            .send(
                "DOM.pushNodesByBackendIdsToFrontend",
                json!({ "backendNodeIds": [backend_node_id] }),
            )
            .await?;
        let node_id = push
            .get("nodeIds")
            .and_then(|ids| ids.get(0))
            .and_then(Value::as_i64);

        let resolved = cdp
            .send(
                "DOM.resolveNode",
                json!({ "backendNodeId": backend_node_id }),
            )
            .await?;
        let object_id = resolved
            .get("object")
            .and_then(|object| object.get("objectId"))
            .and_then(Value::as_str)
            .map(str::to_string);

        // Leave pick mode now that an element has been chosen.
        cdp.send("Overlay.setInspectMode", json!({ "mode": "none" }))
            .await
            .log_err();
        this.update(cx, |this, cx| {
            this.picking = false;
            cx.notify();
        })
        .ok();

        let framework = this
            .read_with(cx, |this, _| this.framework)
            .unwrap_or(Framework::Unknown);

        if let Some(object_id) = object_id {
            let resolution = source_map::resolve(cdp, framework, &object_id).await?;
            this.update(cx, |this, cx| this.handle_resolution(resolution, cx))
                .ok();
        }

        if let Some(node_id) = node_id {
            let cdp = cdp.clone();
            // Loading rules creates one editor per rule, which needs a `Window`; obtain one via the
            // view's window handle.
            if let Ok(window_handle) = this.read_with(cx, |this, _| this.window_handle) {
                let css_panel = this.read_with(cx, |this, _| this.css_panel.clone()).ok();
                if let Some(css_panel) = css_panel {
                    window_handle
                        .update(cx, |_, window, cx| {
                            css_panel.update(cx, |panel, cx| {
                                panel.load_for_node(cdp, node_id, window, cx);
                            });
                        })
                        .ok();
                }
            }
        }

        Ok(())
    }

    fn handle_resolution(&mut self, resolution: Resolution, cx: &mut Context<Self>) {
        match resolution {
            Resolution::Source(location) => self.open_source(location, cx),
            Resolution::SelectorOnly { selector } => {
                log::info!("no deterministic source for element; selector: {selector}");
            }
        }
    }

    /// Open the resolved source file at the exact line/column in the workspace.
    fn open_source(&mut self, location: SourceLocation, cx: &mut Context<Self>) {
        let Some(abs_path) = self.resolve_abs_path(&location.file, cx) else {
            log::warn!("could not locate {} in any worktree", location.file);
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        // Framework line/column are 1-based; Point is 0-based.
        let point = TextPoint::new(
            location.line.saturating_sub(1),
            location.column.saturating_sub(1),
        );
        let window_handle = self.window_handle;

        cx.spawn(async move |_, cx| {
            // `open_abs_path` needs a `Window`; obtain one via the view's window handle.
            let open_task = window_handle.update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.open_abs_path(abs_path, OpenOptions::default(), window, cx)
                })
            })?;
            let item = open_task.await?;
            window_handle.update(cx, |_, window, cx| {
                if let Some(editor) = item.downcast::<editor::Editor>() {
                    editor.update(cx, |editor, cx| {
                        editor.go_to_singleton_buffer_point_silently(point, window, cx);
                    });
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Join a framework-reported (project-relative) file path to whichever visible worktree contains
    /// it, returning an absolute path that exists on disk.
    fn resolve_abs_path(&self, file: &str, cx: &App) -> Option<PathBuf> {
        let relative = PathBuf::from(file);
        for worktree in self.project.read(cx).visible_worktrees(cx) {
            let candidate = worktree.read(cx).abs_path().join(&relative);
            if candidate.exists() {
                return Some(candidate);
            }
        }
        None
    }

    fn toggle_pick_mode(&mut self, cx: &mut Context<Self>) {
        let SessionState::Connected(cdp) = &self.state else {
            return;
        };
        let cdp = cdp.clone();
        self.picking = !self.picking;
        let picking = self.picking;
        cx.background_spawn(async move {
            let mode = if picking { "searchForNode" } else { "none" };
            cdp.send(
                "Overlay.setInspectMode",
                json!({
                    "mode": mode,
                    "highlightConfig": {
                        "showInfo": true,
                        "contentColor": { "r": 111, "g": 168, "b": 220, "a": 0.4 },
                    }
                }),
            )
            .await
            .log_err();
        })
        .detach();
        cx.notify();
    }

    /// Forward a left click in the preview image to the page over CDP.
    fn forward_click(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        // In pick mode Chrome handles selection via the inspect overlay; don't interfere.
        if self.picking {
            return;
        }
        let (Some(frame), Some(bounds)) = (&self.latest_frame, self.image_bounds) else {
            return;
        };
        let SessionState::Connected(cdp) = &self.state else {
            return;
        };

        let image_x = f32::from(event.position.x - bounds.origin.x);
        let image_y = f32::from(event.position.y - bounds.origin.y);
        let (page_x, page_y) = screencast::image_to_page_coords(
            &frame.metadata,
            image_x,
            image_y,
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
        );

        let cdp = cdp.clone();
        cx.background_spawn(async move {
            for event_type in ["mousePressed", "mouseReleased"] {
                cdp.send(
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": event_type,
                        "x": page_x,
                        "y": page_y,
                        "button": "left",
                        "clickCount": 1,
                    }),
                )
                .await
                .log_err();
            }
        })
        .detach();
    }
}

impl Focusable for WebPreviewView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for WebPreviewView {}

impl Item for WebPreviewView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        PRODUCT_NAME.into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<ui::Icon> {
        Some(ui::Icon::new(IconName::Screen))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("looking glass: open")
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event);
    }
}

impl WebPreviewView {
    /// The branded header bar: product mark, status pill, pick-mode toggle, help toggle.
    fn render_header(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();

        let (status_text, status_color) = match &self.state {
            SessionState::Connecting => ("Connecting…".to_string(), Color::Muted),
            SessionState::Connected(_) => {
                (format!("Live · {}", self.framework.label()), Color::Success)
            }
            SessionState::Failed(_) => ("Disconnected".to_string(), Color::Error),
        };

        h_flex()
            .h_8()
            .justify_between()
            .px_2()
            .border_b_1()
            .border_color(colors.border)
            .bg(colors.title_bar_background)
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::Screen)
                            .size(IconSize::Small)
                            .color(Color::Accent),
                    )
                    .child(Label::new(PRODUCT_NAME).weight(FontWeight::SEMIBOLD))
                    .child(
                        // Status pill.
                        h_flex()
                            .gap_1()
                            .ml_1()
                            .px_1p5()
                            .rounded_sm()
                            .bg(colors.element_background)
                            .child(
                                Label::new(status_text)
                                    .size(LabelSize::Small)
                                    .color(status_color),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        IconButton::new("looking-glass-pick", IconName::MagnifyingGlass)
                            .tooltip(Tooltip::text("Pick an element to open its source (⌘⇧I)"))
                            .toggle_state(self.picking)
                            .on_click(
                                cx.listener(|this, _, _window, cx| this.toggle_pick_mode(cx)),
                            ),
                    )
                    .child(
                        IconButton::new("looking-glass-help", IconName::Info)
                            .tooltip(Tooltip::text("Shortcuts & help"))
                            .toggle_state(self.show_help)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.show_help = !this.show_help;
                                cx.notify();
                            })),
                    ),
            )
    }

    /// The empty / onboarding state shown before the first frame arrives.
    fn render_onboarding(&self, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();

        let (headline, detail): (SharedString, SharedString) = match &self.state {
            SessionState::Connecting => (
                "Connecting to your app…".into(),
                "Launching a browser and attaching to your dev server.".into(),
            ),
            SessionState::Failed(error) => ("Couldn't connect".into(), error.clone()),
            SessionState::Connected(_) => (
                "Waiting for the first frame…".into(),
                "Your app is connected. The preview will appear in a moment.".into(),
            ),
        };

        let step = |n: &str, text: &str| {
            h_flex()
                .gap_2()
                .child(
                    div()
                        .flex_none()
                        .size_5()
                        .rounded_full()
                        .bg(colors.element_background)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(Label::new(n.to_string()).size(LabelSize::Small)),
                )
                .child(Label::new(text.to_string()).color(Color::Muted))
        };

        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_4()
            .p_8()
            .child(
                Icon::new(IconName::Screen)
                    .size(IconSize::XLarge)
                    .color(Color::Muted),
            )
            .child(
                v_flex()
                    .items_center()
                    .gap_1()
                    .child(Label::new(headline).weight(FontWeight::SEMIBOLD))
                    .child(
                        div().max_w_96().child(
                            Label::new(detail)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                    ),
            )
            .child(
                v_flex()
                    .gap_1p5()
                    .child(step("1", "Start your dev server (e.g. npm run dev)"))
                    .child(step(
                        "2",
                        "Pick mode (⌘⇧I) → click an element to open its source",
                    ))
                    .child(step("3", "Edit its CSS on the right — changes apply live")),
            )
            .into_any_element()
    }

    /// The in-panel shortcuts/help overlay.
    fn render_help(&self, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();

        let row = |keys: &str, what: &str| {
            h_flex()
                .justify_between()
                .gap_4()
                .child(Label::new(what.to_string()).color(Color::Muted))
                .child(
                    div()
                        .px_1p5()
                        .rounded_sm()
                        .bg(colors.element_background)
                        .child(
                            Label::new(keys.to_string())
                                .size(LabelSize::Small)
                                .buffer_font(cx),
                        ),
                )
        };

        v_flex()
            .absolute()
            .top_8()
            .right_2()
            .w_80()
            .p_3()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.elevated_surface_background)
            .shadow_md()
            .child(Label::new("Shortcuts").weight(FontWeight::SEMIBOLD))
            .child(row("⌘K ⌘⇧V", "Open Looking Glass"))
            .child(row("⌘⇧I", "Toggle pick mode"))
            .child(
                div()
                    .pt_1()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .child(
                        Label::new(
                            "Pick an element to jump to its source. Edit CSS on the right to see \
                             changes live; Write saves to the source file.",
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            .into_any_element()
    }
}

impl Render for WebPreviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Clone so the borrow of `cx` is released before the `&mut cx` uses below.
        let colors = cx.theme().colors().clone();

        let preview = if self.latest_frame.is_some() {
            let image = self.latest_frame.as_ref().map(|frame| frame.image.clone());
            let view = cx.entity().downgrade();
            div()
                .relative()
                .size_full()
                .when_some(image, |this, image| this.child(img(image).size_full()))
                .child(
                    // Capture the rendered image's bounds each layout for click→page mapping.
                    canvas(
                        move |bounds, _window, cx| {
                            view.update(cx, |this, _| this.image_bounds = Some(bounds))
                                .ok();
                        },
                        |_bounds, _, _window, _cx| {},
                    )
                    .absolute()
                    .size_full(),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                        this.forward_click(event, cx);
                    }),
                )
                .into_any_element()
        } else {
            self.render_onboarding(cx)
        };

        // Build sub-elements that borrow `cx` up front, so the builder chain below doesn't
        // re-borrow `cx` while it already holds a listener closure.
        let header = self.render_header(cx).into_any_element();
        let help = self
            .show_help
            .then(|| self.render_help(cx))
            .unwrap_or_else(|| gpui::Empty.into_any_element());

        v_flex()
            .key_context("WebPreview")
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(colors.editor_background)
            .on_action(
                cx.listener(|this, _: &ToggleWebPickMode, _window, cx| this.toggle_pick_mode(cx)),
            )
            .child(header)
            .child(
                h_flex()
                    .size_full()
                    .child(div().flex_1().h_full().child(preview))
                    .child(
                        div()
                            .w(px(320.))
                            .h_full()
                            .border_l_1()
                            .border_color(colors.border)
                            .child(self.css_panel.clone()),
                    ),
            )
            .child(help)
    }
}

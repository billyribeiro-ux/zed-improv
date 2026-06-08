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
mod path_resolve;
mod screencast;
mod source_map;
mod web_preview_settings;

use anyhow::{Context as _, Result};
use cdp::CdpClient;
use css_panel::CssPanel;
use futures::StreamExt as _;
use gpui::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Bounds, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, MouseButton, MouseDownEvent, ObjectFit, Pixels, Task, WeakEntity,
    Window, canvas, div, img, prelude::*, px,
};
use language::Point as TextPoint;
use screencast::DecodedFrame;
use serde_json::{Value, json};
use settings::Settings as _;
use source_map::{Framework, Resolution, SourceLocation};
use std::sync::Arc;
use std::time::Duration;
use ui::{Icon, IconButton, IconName, IconSize, Label, Tooltip, prelude::*};
use util::ResultExt as _;
use web_preview_settings::{DEFAULT_REMOTE_DEBUGGING_PORT, WebPreviewSettings};
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

/// Marker type for deduplicating Looking Glass toasts.
struct WebPreviewNotice;

/// Turn a connection-failure error chain into a single actionable remediation line, so the user
/// knows *how* to fix it rather than reading a raw anyhow chain.
fn remediation_for(error: &str) -> String {
    let lower = error.to_lowercase();
    if lower.contains("dev server") || lower.contains("waiting for dev server") {
        "Your dev server isn't responding. Start it (e.g. `npm run dev`) and check the URL in \
         settings (`web_preview.url`), then Retry."
            .to_string()
    } else if lower.contains("chrome") || lower.contains("chromium") {
        "Couldn't launch Chrome. Install Google Chrome, or set `web_preview.chrome_path` in \
         settings, then Retry."
            .to_string()
    } else if lower.contains("port") || lower.contains("debugging") {
        "Couldn't reach Chrome's debugging port. Another process may be using it — change \
         `web_preview.remote_debugging_port` in settings, then Retry."
            .to_string()
    } else {
        error.to_string()
    }
}

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
    /// Initial connection never succeeded (dev server down, Chrome missing, …).
    Failed(SharedString),
    /// A previously-live connection was lost (Chrome closed, dev server restarted, socket dropped).
    Disconnected,
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
    /// The frame currently being painted, and the one before it. Each decoded frame is a fresh
    /// `RenderImage` that inserts a GPU sprite-atlas tile keyed by its id, so old frames must be
    /// explicitly released with `window.drop_image` or they leak GPU memory for the whole session
    /// (mirrors `livekit_client::RemoteVideoTrackView`).
    current_rendered_frame: Option<Arc<gpui::RenderImage>>,
    previous_rendered_frame: Option<Arc<gpui::RenderImage>>,
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
        // Release any GPU frame textures still retained when the view is dropped.
        cx.on_release(|this, cx| {
            for frame in [
                this.previous_rendered_frame.take(),
                this.current_rendered_frame.take(),
            ]
            .into_iter()
            .flatten()
            {
                this.window_handle
                    .update(cx, |_, window, _| window.drop_image(frame).log_err())
                    .ok();
            }
        })
        .detach();

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
            current_rendered_frame: None,
            previous_rendered_frame: None,
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
    ///
    /// Resets prior session state first, so this also serves reconnect: any old event-loop tasks,
    /// Chrome process, and frame are torn down before a fresh attempt.
    fn connect(&mut self, cx: &mut Context<Self>) {
        self._tasks.clear();
        self._chrome = None;
        self.latest_frame = None;
        self.framework = Framework::Unknown;
        self.state = SessionState::Connecting;
        cx.notify();

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

    /// Re-establish the session from scratch (used by the Retry/Reconnect affordance).
    fn reconnect(&mut self, cx: &mut Context<Self>) {
        self.connect(cx);
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
        // Unique profile dir + port per session so two panels (or a stale browser) never collide.
        // A non-default configured port is treated as an explicit override; otherwise pick a free one.
        let session_key = this.entity_id().as_u64();
        let user_data_dir =
            std::env::temp_dir().join(format!("zed-web-preview-chrome-{session_key}"));
        let port = if settings.remote_debugging_port == DEFAULT_REMOTE_DEBUGGING_PORT {
            chrome::free_port()?
        } else {
            settings.remote_debugging_port
        };
        let process = chrome::launch(
            chrome_path,
            &settings.url,
            port,
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

        // Disconnect watcher: when the CDP socket closes (Chrome exits, dev server restarts, network
        // drop), flip the view into `Disconnected` so the header pill turns red and the preview
        // offers a Reconnect affordance instead of sitting frozen on a stale "Live" frame.
        if let Some(closed) = cdp.take_closed_signal() {
            let disconnect_task = cx.spawn(async move |this, cx| {
                let _ = closed.await;
                this.update(cx, |this, cx| {
                    if matches!(this.state, SessionState::Connected(_)) {
                        this.state = SessionState::Disconnected;
                        this.picking = false;
                        cx.notify();
                    }
                })
                .ok();
            });
            self._tasks.push(disconnect_task);
        }

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
        // Recover from an early probe (ran before hydration) or an SPA route change by re-detecting
        // when we still think the framework is Unknown.
        let framework = source_map::detect_framework_if_unknown(cdp, framework).await;
        this.update(cx, |this, _| this.framework = framework).ok();

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
            Resolution::SelectorOnly { selector, hint } => {
                let message = hint.unwrap_or_else(|| {
                    "No source mapping for this element — its styles are loaded on the right."
                        .to_string()
                });
                log::info!("{message} (selector: {selector})");
                self.notify_user(message, cx);
            }
        }
    }

    /// Surface a short, transient message to the user via a workspace toast.
    fn notify_user(&self, message: String, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(
                    workspace::Toast::new(
                        workspace::notifications::NotificationId::unique::<WebPreviewNotice>(),
                        message,
                    ),
                    cx,
                );
            })
            .ok();
    }

    /// Open the resolved source file at the exact line/column in the workspace.
    fn open_source(&mut self, location: SourceLocation, cx: &mut Context<Self>) {
        let Some(abs_path) = path_resolve::resolve(&self.project, &location.file, cx) else {
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
        let (Some(frame), Some(pane_bounds)) = (&self.latest_frame, self.image_bounds) else {
            return;
        };
        let SessionState::Connected(cdp) = &self.state else {
            return;
        };

        // The frame is painted with `ObjectFit::Contain` (letterboxed) inside the pane, so the image
        // occupies only a sub-rect. Compute that rect the same way `img` does and map the click
        // against it; clicks in the letterbox margin are outside the page and are ignored.
        let image_bounds = ObjectFit::Contain.get_bounds(pane_bounds, frame.image.size(0));
        if !image_bounds.contains(&event.position) {
            return;
        }
        let image_x = f32::from(event.position.x - image_bounds.origin.x);
        let image_y = f32::from(event.position.y - image_bounds.origin.y);
        let Some((page_x, page_y)) = screencast::image_to_page_coords(
            &frame.metadata,
            image_x,
            image_y,
            f32::from(image_bounds.size.width),
            f32::from(image_bounds.size.height),
        ) else {
            return;
        };

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
            SessionState::Failed(_) => ("Couldn't connect".to_string(), Color::Error),
            SessionState::Disconnected => ("Disconnected".to_string(), Color::Error),
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
    fn render_onboarding(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();

        // Failed/Disconnected states get a tailored, actionable headline + a Retry affordance.
        let recoverable = matches!(
            self.state,
            SessionState::Failed(_) | SessionState::Disconnected
        );
        let (headline, detail): (SharedString, SharedString) = match &self.state {
            SessionState::Connecting => (
                "Connecting to your app…".into(),
                "Launching a browser and attaching to your dev server.".into(),
            ),
            SessionState::Failed(error) => {
                ("Couldn't connect".into(), remediation_for(error).into())
            }
            SessionState::Disconnected => (
                "Connection lost".into(),
                "The browser or dev server stopped. Reconnect to pick up where you left off."
                    .into(),
            ),
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
            .when(recoverable, |this| {
                this.child(
                    ui::Button::new("web-preview-retry", "Retry")
                        .on_click(cx.listener(|this, _, _window, cx| this.reconnect(cx))),
                )
            })
            .when(!recoverable, |this| {
                this.child(
                    v_flex()
                        .gap_1p5()
                        .child(step("1", "Start your dev server (e.g. npm run dev)"))
                        .child(step(
                            "2",
                            "Pick mode (⌘⇧I) → click an element to open its source",
                        ))
                        .child(step("3", "Edit its CSS on the right — changes apply live")),
                )
            })
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Clone so the borrow of `cx` is released before the `&mut cx` uses below.
        let colors = cx.theme().colors().clone();

        let preview =
            if let Some(image) = self.latest_frame.as_ref().map(|frame| frame.image.clone()) {
                // Rotate retained frames and release the GPU texture of the one two frames back, so the
                // screencast doesn't leak a sprite-atlas tile per frame (livekit pattern).
                if let Some(current) = self.current_rendered_frame.take() {
                    if let Some(previous) = self.previous_rendered_frame.take() {
                        if previous.id != current.id {
                            window.drop_image(previous).log_err();
                        }
                    }
                    self.previous_rendered_frame = Some(current);
                }
                self.current_rendered_frame = Some(image.clone());

                let view = cx.entity().downgrade();
                div()
                    .relative()
                    .size_full()
                    .child(img(image).size_full())
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

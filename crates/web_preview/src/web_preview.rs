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

// Modules are `pub` so `examples/doctor.rs` (the end-to-end diagnostic binary) can drive the real
// pipeline; they are not a stable API.
pub mod cdp;
pub mod chrome;
mod css_panel;
pub mod dev_server;
pub mod path_resolve;
pub mod screencast;
pub mod source_map;
pub mod web_preview_settings;

use anyhow::{Context as _, Result};
use cdp::CdpClient;
use css_panel::CssPanel;
use futures::StreamExt as _;
use gpui::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Bounds, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, MouseButton, MouseDownEvent, MouseMoveEvent, ObjectFit, Pixels,
    ScrollWheelEvent, Task, WeakEntity, Window, canvas, div, img, prelude::*, px,
};
use language::Point as TextPoint;
use screencast::DecodedFrame;
use serde_json::{Value, json};
use settings::Settings as _;
use source_map::{Framework, Resolution, SourceLocation};
use std::sync::Arc;
use std::time::Duration;
use ui::{CommonAnimationExt, Icon, IconButton, IconName, IconSize, Label, Tooltip, prelude::*};
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
        /// Reload the previewed page.
        ReloadWebPreview,
    ]
);

/// User-facing product name for the panel.
const PRODUCT_NAME: &str = "Looking Glass";

/// Marker type for deduplicating Looking Glass toasts.
struct WebPreviewNotice;

fn viewport_scale_key(scale: f32) -> u32 {
    (scale.max(1.0) * 1000.0).round() as u32
}

fn viewport_metrics(css_width: u32, css_height: u32, scale: f32) -> (u32, u32, u32) {
    (
        css_width.max(1),
        css_height.max(1),
        viewport_scale_key(scale),
    )
}

fn screencast_bounds(css_width: u32, css_height: u32, scale: f32) -> (u32, u32) {
    (
        (css_width.max(1) as f32 * scale.max(1.0)).round() as u32,
        (css_height.max(1) as f32 * scale.max(1.0)).round() as u32,
    )
}

/// Pane-relative rect at which a frame of `frame_px` device pixels must be painted to be sampled
/// 1:1 (sharp): exactly `frame_px / scale` logical pixels, centered. GPUI snaps a quad's device
/// size from its logical size, so an integer device-pixel size survives snapping regardless of
/// fractional origin — whereas fitting the image to the (fractional) pane can disagree with the
/// texture by ±1 device pixel and put the whole preview into a permanent bilinear resample.
/// Returns `None` when the frame doesn't fit the pane (mid-resize; caller falls back to a scaled
/// fit). The 1px tolerance covers Chrome rounding capture sizes at fractional scales.
fn pixel_exact_placement(
    frame_px: gpui::Size<gpui::DevicePixels>,
    pane: gpui::Size<Pixels>,
    scale: f32,
) -> Option<Bounds<Pixels>> {
    let scale = scale.max(0.1);
    let target_width = frame_px.width.0 as f32 / scale;
    let target_height = frame_px.height.0 as f32 / scale;
    let pane_width = f32::from(pane.width);
    let pane_height = f32::from(pane.height);
    (target_width <= pane_width + 1.0 && target_height <= pane_height + 1.0).then(|| {
        Bounds::new(
            gpui::point(
                px((pane_width - target_width) / 2.0),
                px((pane_height - target_height) / 2.0),
            ),
            gpui::size(px(target_width), px(target_height)),
        )
    })
}

/// Map a GPUI keystroke to the CDP `dispatchKeyEvent` fields `(key, code, windowsVirtualKeyCode,
/// text)`. `text` is `Some` for printable characters (which insert text) and `None` for control keys.
fn key_to_cdp(ks: &gpui::Keystroke) -> (String, String, u32, Option<String>) {
    // Non-printable / named keys → explicit DOM key/code/virtual-key-code. text is None.
    let named: Option<(&str, &str, u32)> = match ks.key.as_str() {
        "enter" => Some(("Enter", "Enter", 13)),
        "tab" => Some(("Tab", "Tab", 9)),
        "backspace" => Some(("Backspace", "Backspace", 8)),
        "delete" => Some(("Delete", "Delete", 46)),
        "escape" => Some(("Escape", "Escape", 27)),
        "up" => Some(("ArrowUp", "ArrowUp", 38)),
        "down" => Some(("ArrowDown", "ArrowDown", 40)),
        "left" => Some(("ArrowLeft", "ArrowLeft", 37)),
        "right" => Some(("ArrowRight", "ArrowRight", 39)),
        "home" => Some(("Home", "Home", 36)),
        "end" => Some(("End", "End", 35)),
        "pageup" => Some(("PageUp", "PageUp", 33)),
        "pagedown" => Some(("PageDown", "PageDown", 34)),
        _ => None,
    };
    if let Some((key, code, vk)) = named {
        return (key.to_string(), code.to_string(), vk, None);
    }
    // Space is printable: it must carry `text` or Chrome inserts nothing (a `rawKeyDown` without
    // text types no character — verified). GPUI reports it as the named key "space", so the
    // generic key_char path below would miss it.
    if ks.key == "space" {
        return (" ".into(), "Space".into(), 32, Some(" ".into()));
    }

    // Printable: use the character the OS produced (respects shift/layout). Derive a plausible
    // `code` and virtual-key-code for single ASCII letters/digits.
    let text = ks.key_char.clone().filter(|c| !c.is_empty());
    let key = text.clone().unwrap_or_else(|| ks.key.clone());
    let first = key.chars().next().unwrap_or('\0');
    let (code, vk) = if first.is_ascii_alphabetic() {
        (
            format!("Key{}", first.to_ascii_uppercase()),
            first.to_ascii_uppercase() as u32,
        )
    } else if first.is_ascii_digit() {
        (format!("Digit{first}"), first as u32)
    } else {
        (String::new(), first as u32)
    };
    (key, code, vk, text)
}

/// Classify a session-failure error into a bounded stage label for telemetry. Returns a fixed set
/// of strings (never raw error text) so the telemetry label cardinality stays bounded.
fn failure_stage(error: &anyhow::Error) -> &'static str {
    let text = format!("{error:#}").to_lowercase();
    if text.contains("dev server") {
        "dev_server_unreachable"
    } else if text.contains("chrome") || text.contains("chromium") {
        "chrome_launch"
    } else if text.contains("port") || text.contains("debugging") {
        "debug_port"
    } else if text.contains("cdp") || text.contains("websocket") {
        "cdp_connect"
    } else if text.contains("screencast") {
        "screencast"
    } else {
        "other"
    }
}

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

        // Workspace-level pick-mode toggle, so the pick → edit source → pick-again loop works
        // while focus is in an editor. The panel-context ⌘⇧I binding is shadowed there by
        // `editor::Format` (deepest context wins), which both broke re-entry AND reformatted the
        // user's file; this handler is reached via its own non-colliding chord.
        workspace.register_action(move |workspace, _: &ToggleWebPickMode, window, cx| {
            let view = workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<WebPreviewView>())
                .or_else(|| workspace.items_of_type::<WebPreviewView>(cx).next());
            if let Some(view) = view {
                window.focus(&view.focus_handle(cx), cx);
                view.update(cx, |view, cx| view.toggle_pick_mode(cx));
            }
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
    /// Bounds of the *contained* (letterboxed) preview image, captured each layout for click→page
    /// coordinate mapping. Kept lock-step with where `img` actually paints.
    image_bounds: Option<Bounds<Pixels>>,
    /// The preview pane's full (fractional) bounds from the last layout. The frame is painted at
    /// exactly `frame_device_px / scale` logical pixels centered in this rect, so the snapped GPU
    /// quad equals the texture size — any ±1 device-pixel mismatch (e.g. from sizing the image to
    /// the pane with `ObjectFit`) puts the whole preview into a permanent one-texel bilinear
    /// resample, i.e. visible blur.
    preview_bounds: Option<Bounds<Pixels>>,
    /// Last in-page coordinate we mapped to, used as the scroll fallback when the cursor is over the
    /// letterbox margin (so scroll never silently drops) and to de-dup `mouseMoved` forwarding.
    last_page_point: Option<(f32, f32)>,
    /// The panel's current size in (CSS width, CSS height, device scale), captured every layout from
    /// an always-present canvas so resize is detected even when no new frames are streaming.
    panel_px: Option<(u32, u32, f32)>,
    /// The viewport size actually confirmed-pushed to Chrome. Set only AFTER the override lands, so a
    /// dropped/failed override is retried rather than skipped by the guard.
    viewport_metrics: Option<(u32, u32, u32)>,
    /// The window's scale factor at view creation, passed to Chrome as
    /// `--force-device-scale-factor` so the screencast captures at physical (retina) resolution.
    /// Launch-time only: Chrome's window scale can't be changed at runtime.
    launch_scale: f32,
    /// Debounce task for viewport re-sync on resize.
    _viewport_task: Option<Task<()>>,
    css_panel: Entity<CssPanel>,
    /// Shared `styleSheetId` → header map, populated from `CSS.styleSheetAdded`; used by the CSS
    /// panel to decide whether an edited rule can be written back to source.
    style_sheets: css_panel::StyleSheetRegistry,
    /// The launched Chrome process; kept alive (and killed on drop) for the view's lifetime.
    _chrome: Option<chrome::ChromeProcess>,
    _tasks: Vec<Task<()>>,
    /// Window-bounds observer that drives viewport re-sync on resize.
    _bounds_observer: Option<gpui::Subscription>,
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
            preview_bounds: None,
            last_page_point: None,
            panel_px: None,
            viewport_metrics: None,
            launch_scale: window.scale_factor(),
            _viewport_task: None,
            css_panel,
            style_sheets,
            _chrome: None,
            _tasks: Vec::new(),
            _bounds_observer: None,
        };
        // A window resize doesn't produce a new screencast frame (Chrome streams on page-pixel
        // change, not on a clock), so the canvas-prepaint path wouldn't re-run on resize. Observe
        // window bounds directly to push the new viewport to Chrome and force a re-render.
        this._bounds_observer = Some(cx.observe_window_bounds(window, |this, window, cx| {
            if let Some((w, h, _)) = this.panel_px {
                this.sync_viewport(w, h, window.scale_factor(), cx);
            }
            cx.notify();
        }));
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
        // Reset the synced-viewport record so the fresh session re-pushes the viewport even if the
        // panel is the same size (otherwise the size guard suppresses the override and the page
        // renders at the wrong size after a Retry).
        self.viewport_metrics = None;
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
                        telemetry::event!(
                            "Looking Glass Session Failed",
                            stage = failure_stage(&error)
                        );
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
        // If a dev command is configured and the URL isn't up yet, launch it before waiting.
        if let Some(command) = settings.dev_command.as_deref() {
            let reachable = dev_server::is_reachable(&settings.url, http_client.clone()).await;
            if !reachable {
                if let Ok(project) = this.read_with(cx, |this, _| this.project.clone()) {
                    dev_server::spawn_dev_command(command, &project, cx).await;
                }
            }
        }

        dev_server::wait_until_ready(
            &settings.url,
            http_client.clone(),
            executor.clone(),
            Duration::from_secs(settings.dev_server_timeout_secs),
        )
        .await
        .context("waiting for dev server")?;
        telemetry::event!("Looking Glass Session Stage", stage = "dev_server_ready");

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
        let launch_scale = this
            .read_with(cx, |this, _| this.launch_scale)
            .unwrap_or(1.0);
        let process = chrome::launch(
            chrome_path,
            &settings.url,
            port,
            user_data_dir,
            launch_scale,
            http_client,
            executor,
        )
        .await
        .context("launching chrome")?;

        telemetry::event!("Looking Glass Session Stage", stage = "chrome_launched");

        let ws_url = process.ws_url.clone();
        let cdp = CdpClient::connect(ws_url, cx)
            .await
            .context("connecting CDP")?;
        telemetry::event!("Looking Glass Session Stage", stage = "cdp_connected");

        // We're attached at the BROWSER endpoint (survives page reloads). Auto-attach to page targets
        // so page-scoped commands/events route via the page session — and so a reload, which
        // recreates the page target, simply re-attaches instead of killing the whole session.
        cdp.send(
            "Target.setAutoAttach",
            json!({ "autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true }),
        )
        .await
        .context("Target.setAutoAttach")?;
        // Wait for the page session to be captured (from Target.attachedToTarget) before issuing any
        // page-scoped command, or those commands would go unstamped and fail.
        for _ in 0..50 {
            if cdp.page_session().is_some() {
                break;
            }
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
        }
        if cdp.page_session().is_none() {
            anyhow::bail!("no page target attached over CDP");
        }

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

        // Honor an explicit framework override; otherwise auto-detect. The probe can run before the
        // app has hydrated (no `__svelte_meta`/fibers on nodes yet), so retry a few times with a
        // short delay rather than caching a premature "unknown" (which left the header stuck on
        // "unknown framework" and disabled source resolution).
        let framework = match settings.framework_override {
            Some(framework) => framework,
            None => {
                let mut detected = Framework::Unknown;
                for attempt in 0..10 {
                    detected = source_map::detect_framework(&cdp).await;
                    if detected != Framework::Unknown {
                        break;
                    }
                    if attempt < 9 {
                        cx.background_executor()
                            .timer(Duration::from_millis(300))
                            .await;
                    }
                }
                detected
            }
        };
        this.update(cx, |this, _| this.framework = framework).ok();
        telemetry::event!(
            "Looking Glass Session Stage",
            stage = "framework_detected",
            framework = framework.label()
        );

        // Set the layout viewport to the panel's CSS-pixel size BEFORE starting the screencast, so
        // the page lays out at the size the user sees instead of Chrome's 800×600 default. The
        // physical-resolution (sharp) frames come from `--force-device-scale-factor` at launch —
        // overriding deviceScaleFactor here alone still yields CSS-sized frames, and lying to
        // Chrome with a physical-pixel CSS viewport breaks responsive layout and hit testing.
        let (vp_w, vp_h, vp_scale) = this
            .read_with(cx, |this, _| this.panel_px)
            .ok()
            .flatten()
            .unwrap_or((1280, 800, launch_scale));
        let (capture_w, capture_h) = screencast_bounds(vp_w, vp_h, vp_scale);
        screencast::set_viewport(&cdp, vp_w, vp_h, vp_scale)
            .await
            .context("setting viewport")?;
        this.update(cx, |this, _| {
            this.viewport_metrics = Some(viewport_metrics(vp_w, vp_h, vp_scale))
        })
        .ok();

        // Subscribe to frames and loads BEFORE starting the screencast. A static (already settled)
        // page emits exactly ONE frame — at screencast start — and subscribing afterwards loses the
        // race against the read pump, leaving the preview on "Waiting for the first frame…" forever
        // with every click ignored (verified against Chrome 149).
        let frames = cdp.subscribe("Page.screencastFrame");
        let loads = cdp.subscribe("Page.loadEventFired");

        screencast::start(&cdp, settings.screencast_quality, capture_w, capture_h)
            .await
            .context("starting screencast")?;
        this.update(cx, |this, cx| {
            this.spawn_event_loops(cdp.clone(), frames, loads, cx);
        })
        .ok();

        Ok(cdp)
    }

    /// Route the CDP events we care about back onto the view. The frame and load streams are
    /// subscribed by the caller BEFORE the screencast starts (see `establish_session`) so the
    /// single first frame of a static page can't be dropped. Runs in the view's own context so the
    /// spawned tasks can update `self`.
    fn spawn_event_loops(
        &mut self,
        cdp: CdpClient,
        frames: futures::channel::mpsc::UnboundedReceiver<Value>,
        loads: futures::channel::mpsc::UnboundedReceiver<Value>,
        cx: &mut Context<Self>,
    ) {
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

        // Reload survival: a page reload (F5 / HMR full reload / navigation) STOPS the screencast and
        // clears the viewport override (verified against real Chrome — frames stop flowing until
        // re-issued). Re-apply viewport + screencast and re-detect the framework on every load so the
        // preview keeps working after the user refreshes, instead of freezing on a dead frame.
        let mut loads = loads;
        let load_cdp = cdp.clone();
        let load_task = cx.spawn(async move |this, cx| {
            while loads.next().await.is_some() {
                log::info!("web preview: page loaded — re-initializing session");
                // Re-issue viewport at the panel's captured size and restart the screencast.
                let (vp, quality) = this
                    .read_with(cx, |this, cx| {
                        let settings = WebPreviewSettings::get_global(cx);
                        (this.panel_px, settings.screencast_quality)
                    })
                    .unwrap_or((None, 80u8));
                let (css_w, css_h, scale) = vp.unwrap_or((1280, 800, 2.0));
                let (cap_w, cap_h) = screencast_bounds(css_w, css_h, scale);
                if screencast::set_viewport(&load_cdp, css_w, css_h, scale)
                    .await
                    .log_err()
                    .is_none()
                {
                    continue;
                }
                screencast::restart(&load_cdp, quality, cap_w, cap_h).await;
                // Re-detect framework (a reload re-runs the app; metadata reappears after hydration).
                let framework = source_map::detect_framework(&load_cdp).await;
                this.update(cx, |this, cx| {
                    this.viewport_metrics = Some(viewport_metrics(css_w, css_h, scale));
                    if framework != Framework::Unknown {
                        this.framework = framework;
                    }
                    cx.notify();
                })
                .ok();
            }
        });
        self._tasks.push(load_task);

        // Screencast frames: decode on a background thread, apply on the foreground. This is the last
        // consumer of `cdp`, so move it in.
        let mut frames = frames;
        let frame_cdp = cdp;
        // Client-side frame pacing: Chrome streams unthrottled (50+ fps on an animating page), and
        // each frame costs a full JPEG decode plus a multi-MiB GPU texture upload — the dominant
        // CPU/GPU load of the whole feature. Cap the decode/upload rate (~30 fps, divided further by
        // the every-Nth setting) and let the drain loop below skip to the freshest frame; Chrome's
        // ack budget then paces the stream itself. The FIRST frame is never delayed, so static
        // pages still paint immediately.
        let every_nth = WebPreviewSettings::get_global(cx)
            .screencast_every_nth_frame
            .max(1);
        let frame_interval = Duration::from_millis(33) * every_nth;
        let frame_task = cx.spawn(async move |this, cx| {
            let mut first_frame_rendered = false;
            while let Some(mut params) = frames.next().await {
                // Drain to the latest queued frame: if we've fallen behind (decode/render slower than
                // the stream), skip stale frames and only process the freshest, so the preview never
                // lags. Ack each skipped frame so Chrome keeps sending.
                while let Ok(newer) = frames.try_recv() {
                    if let Some(sid) = screencast::session_id(&params) {
                        screencast::ack_no_reply(&frame_cdp, sid);
                    }
                    params = newer;
                }
                // Ack the frame we're about to render (fire-and-forget — the ack has no useful
                // result, and registering a pending slot per frame is pure overhead).
                if let Some(session_id) = screencast::session_id(&params) {
                    screencast::ack_no_reply(&frame_cdp, session_id);
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
                        if first_frame_rendered {
                            cx.background_executor().timer(frame_interval).await;
                        }
                        first_frame_rendered = true;
                    }
                    Err(error) => log::warn!("screencast frame decode failed: {error:#}"),
                }
            }
        });
        self._tasks.push(frame_task);
        // Pick mode no longer relies on a CDP event subscription: the inspect overlay's
        // `inspectNodeRequested` does NOT fire for synthesized clicks in headless Chrome (verified).
        // Instead, a click in pick mode resolves the element directly via `DOM.getNodeForLocation`
        // (see `pick_at` / `forward_click`).
    }

    /// In pick mode, resolve the element at a page coordinate and open its source.
    fn pick_at(&mut self, page_x: f32, page_y: f32, cx: &mut Context<Self>) {
        log::info!("web preview: pick_at document coords ({page_x}, {page_y})");
        let SessionState::Connected(cdp) = &self.state else {
            log::info!("web preview: pick ignored — not connected");
            return;
        };
        let cdp = cdp.clone();
        let task = cx.spawn(async move |this, cx| {
            WebPreviewView::resolve_and_open(&cdp, &this, page_x, page_y, cx)
                .await
                .log_err();
        });
        self._tasks.push(task);
        self.picking = false;
        cx.notify();
    }

    async fn resolve_and_open(
        cdp: &CdpClient,
        this: &WeakEntity<Self>,
        page_x: f32,
        page_y: f32,
        cx: &mut gpui::AsyncApp,
    ) -> Result<()> {
        // Hit-test the element at the page coordinate derived from the current screencast metadata.
        // In the Chrome versions we target, DOM.getNodeForLocation accounts for page scroll when
        // given that document-space point.
        let located = match cdp
            .send(
                "DOM.getNodeForLocation",
                json!({
                    "x": page_x as i64,
                    "y": page_y as i64,
                    "includeUserAgentShadowDOM": false,
                }),
            )
            .await
        {
            Ok(located) => located,
            Err(error) => {
                this.update(cx, |this, cx| {
                    this.notify_user(
                        format!("Couldn't resolve an element at that point: {error}"),
                        cx,
                    );
                })
                .ok();
                return Ok(());
            }
        };
        let Some(backend_node_id) = located.get("backendNodeId").and_then(Value::as_i64) else {
            this.update(cx, |this, cx| {
                this.notify_user(
                    "Couldn't resolve an element at that point — try again.".to_string(),
                    cx,
                );
            })
            .ok();
            return Ok(());
        };
        // `DOM.getNodeForLocation` only returns a frontend `nodeId` once a document has been
        // requested in this session — without `DOM.getDocument` it returns just the backend id and
        // the CSS panel (which needs a `nodeId`) never loads (verified against Chrome 149). Request
        // the document (depth 0 — we don't need the tree), then push the backend id to get a real
        // `nodeId`.
        let node_id = match cdp.send("DOM.getDocument", json!({ "depth": 0 })).await {
            Ok(_) => cdp
                .send(
                    "DOM.pushNodesByBackendIdsToFrontend",
                    json!({ "backendNodeIds": [backend_node_id] }),
                )
                .await
                .log_err()
                .and_then(|pushed| {
                    pushed
                        .get("nodeIds")
                        .and_then(Value::as_array)
                        .and_then(|ids| ids.first())
                        .and_then(Value::as_i64)
                })
                .filter(|id| *id != 0),
            Err(error) => {
                log::warn!("web preview: DOM.getDocument failed; styles panel skipped: {error:#}");
                None
            }
        };

        // Highlight it briefly so the user sees what was picked.
        cdp.send(
            "Overlay.highlightNode",
            json!({
                "backendNodeId": backend_node_id,
                "highlightConfig": {
                    "showInfo": true,
                    "contentColor": { "r": 111, "g": 168, "b": 220, "a": 0.35 },
                    "borderColor": { "r": 111, "g": 168, "b": 220, "a": 0.9 },
                }
            }),
        )
        .await
        .log_err();

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
        } else {
            this.update(cx, |this, cx| {
                this.notify_user(
                    "Couldn't inspect the picked element — try clicking a rendered DOM element."
                        .to_string(),
                    cx,
                );
            })
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
            Resolution::Source(location) => {
                telemetry::event!(
                    "Looking Glass Pick",
                    outcome = "source",
                    framework = self.framework.label()
                );
                self.open_source(location, cx)
            }
            Resolution::SelectorOnly { selector, hint } => {
                telemetry::event!(
                    "Looking Glass Pick",
                    outcome = "selector_only",
                    framework = self.framework.label()
                );
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
            let message = format!(
                "Couldn't find {} in this project. If you moved the project, restart the dev server.",
                location.file
            );
            log::warn!("{message}");
            self.notify_user(message, cx);
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
        let reported_file = location.file;
        let window_handle = self.window_handle;
        let workspace_for_error = workspace.clone();

        cx.spawn(async move |_, cx| {
            let result = async {
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
            }
            .await;

            if let Err(error) = result {
                let message = format!("Couldn't open {reported_file}: {error:#}");
                log::warn!("{message}");
                workspace_for_error.update(cx, |workspace, cx| {
                    workspace.show_toast(
                        workspace::Toast::new(
                            workspace::notifications::NotificationId::unique::<WebPreviewNotice>(),
                            message,
                        ),
                        cx,
                    );
                });
            }
        })
        .detach();
    }

    fn toggle_pick_mode(&mut self, cx: &mut Context<Self>) {
        self.set_pick_mode(!self.picking, cx);
    }

    /// Cancel pick mode if active (bound to Esc). When not picking, the event is propagated so
    /// Escape still reaches outer handlers AND `forward_key` can deliver it to the previewed page
    /// (e.g. to close a modal in the user's app) — consuming it unconditionally ate every Escape.
    fn cancel_pick_mode(&mut self, cx: &mut Context<Self>) {
        if self.picking {
            self.set_pick_mode(false, cx);
        } else {
            cx.propagate();
        }
    }

    fn set_pick_mode(&mut self, picking: bool, cx: &mut Context<Self>) {
        if !matches!(self.state, SessionState::Connected(_)) {
            return;
        }
        // Pick mode is a local flag: a subsequent click is resolved via DOM.getNodeForLocation
        // (`forward_click` → `pick_at`). We deliberately do NOT use Overlay.setInspectMode — its
        // inspectNodeRequested event never fires for synthesized clicks in headless Chrome.
        self.picking = picking;
        // Clear any leftover highlight when leaving pick mode.
        if !picking {
            if let SessionState::Connected(cdp) = &self.state {
                let cdp = cdp.clone();
                cdp.send_no_reply("Overlay.hideHighlight", Value::Null);
            }
        }
        cx.notify();
    }

    /// Reload the previewed page over CDP.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let SessionState::Connected(cdp) = &self.state else {
            return;
        };
        let cdp = cdp.clone();
        cx.background_spawn(async move {
            cdp.send("Page.reload", json!({ "ignoreCache": false }))
                .await
                .log_err();
        })
        .detach();
    }

    /// Forward a left click in the preview image to the page over CDP.
    /// Map a window-local mouse position to page CSS coordinates. `image_bounds` is the *contained*
    /// image rect (captured in layout), so this maps directly against where the pixels are painted.
    /// Returns `None` if there's no live frame or the point is outside the rendered page.
    fn page_coords(&self, position: gpui::Point<Pixels>) -> Option<(f32, f32)> {
        let frame = self.latest_frame.as_ref()?;
        let image_bounds = self.image_bounds?;
        if !image_bounds.contains(&position) {
            return None;
        }
        screencast::image_to_page_coords(
            &frame.metadata,
            f32::from(position.x - image_bounds.origin.x),
            f32::from(position.y - image_bounds.origin.y),
            f32::from(image_bounds.size.width),
            f32::from(image_bounds.size.height),
        )
    }

    /// Record the panel's current layout size and, if it changed, push it to Chrome as the page
    /// viewport (so the page lays out — and the screencast streams — at the size the user sees, fixing
    /// blurriness, wrong-layout clicks, and resize). Called every layout from an always-present
    /// canvas, so it fires on resize even when no frames are flowing. Debounced.
    fn sync_viewport(
        &mut self,
        css_width: u32,
        css_height: u32,
        scale: f32,
        cx: &mut Context<Self>,
    ) {
        let css_width = css_width.max(1);
        let css_height = css_height.max(1);
        self.panel_px = Some((css_width, css_height, scale));

        // Skip only if we've actually CONFIRMED these metrics to Chrome (set after the override
        // lands), and we're connected.
        let metrics = viewport_metrics(css_width, css_height, scale);
        if self.viewport_metrics == Some(metrics) {
            return;
        }
        let SessionState::Connected(cdp) = &self.state else {
            return;
        };
        let cdp = cdp.clone();
        let executor = cx.background_executor().clone();
        let quality = WebPreviewSettings::get_global(cx).screencast_quality;
        let (capture_w, capture_h) = screencast_bounds(css_width, css_height, scale);
        // Debounce ~150ms so a resize drag issues one override at the end, not hundreds.
        self._viewport_task = Some(cx.spawn(async move |this, cx| {
            executor.timer(Duration::from_millis(150)).await;
            if screencast::set_viewport(&cdp, css_width, css_height, scale)
                .await
                .log_err()
                .is_none()
            {
                return;
            }
            screencast::restart(&cdp, quality, capture_w, capture_h).await;
            // Mark confirmed ONLY after the override actually landed.
            this.update(cx, |this, _| this.viewport_metrics = Some(metrics))
                .ok();
        }));
    }

    fn forward_click(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        log::info!(
            "web preview: forward_click fired at window {:?}, picking={}, image_bounds={:?}",
            event.position,
            self.picking,
            self.image_bounds
        );
        let Some((page_x, page_y)) = self.page_coords(event.position) else {
            log::info!(
                "web preview: click mapped to None (outside page area), ignored; frame={:?}, pane={:?}",
                self.latest_frame.as_ref().map(|frame| frame.image.size(0)),
                self.preview_bounds,
            );
            return;
        };
        log::info!("web preview: click -> page coords ({page_x}, {page_y})");
        self.last_page_point = Some((page_x, page_y));

        // In pick mode, resolve the element under the click directly (DOM.getNodeForLocation works
        // headlessly; the inspect-overlay's inspectNodeRequested does NOT) and open its source.
        // getNodeForLocation wants DOCUMENT coords, so add the current scroll offset back (page_coords
        // returns VIEWPORT coords).
        if self.picking {
            let (scroll_x, scroll_y) = self
                .latest_frame
                .as_ref()
                .map(|f| (f.metadata.scroll_offset_x, f.metadata.scroll_offset_y))
                .unwrap_or((0.0, 0.0));
            self.pick_at(page_x + scroll_x, page_y + scroll_y, cx);
            return;
        }
        let SessionState::Connected(cdp) = &self.state else {
            return;
        };
        let cdp = cdp.clone();
        cx.background_spawn(async move {
            // Prime hover at the point first (real browsers move the pointer before pressing, so
            // :hover / mousemove-gated UI is in the right state), then press and release with the
            // correct `buttons` bitmask.
            cdp.send_no_reply(
                "Input.dispatchMouseEvent",
                json!({ "type": "mouseMoved", "x": page_x, "y": page_y, "button": "none", "buttons": 0 }),
            );
            for (event_type, buttons) in [("mousePressed", 1), ("mouseReleased", 0)] {
                cdp.send(
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": event_type,
                        "x": page_x,
                        "y": page_y,
                        "button": "left",
                        "buttons": buttons,
                        "clickCount": 1,
                    }),
                )
                .await
                .log_err();
            }
        })
        .detach();
    }

    /// Forward a scroll wheel event to the page so the user can scroll the preview.
    fn forward_scroll(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        log::info!("web preview: forward_scroll fired, delta={:?}", event.delta);
        if self.picking {
            return;
        }
        let SessionState::Connected(cdp) = &self.state else {
            log::info!("web preview: scroll ignored — not connected");
            return;
        };
        // Scroll has no per-pixel target — the user scrolls "the page". If the cursor is over the
        // letterbox margin, fall back to the last in-page point (or page center) instead of dropping
        // the event, which is the dominant "feels dead" cause.
        let (page_x, page_y) = self
            .page_coords(event.position)
            .or(self.last_page_point)
            .unwrap_or((1.0, 1.0));

        // GPUI scroll deltas → pixels. macOS natural-scroll sign is already correct; CDP `mouseWheel`
        // follows the DOM wheel convention (positive deltaY scrolls content down), matching GPUI, so
        // NO negation (verified against real headless Chrome). Both axes must always be present.
        let delta = event.delta.pixel_delta(px(20.0));
        let (delta_x, delta_y) = (f32::from(delta.x), f32::from(delta.y));
        if delta_x == 0.0 && delta_y == 0.0 {
            return;
        }

        let cdp = cdp.clone();
        cx.background_spawn(async move {
            cdp.send(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseWheel",
                    "x": page_x,
                    "y": page_y,
                    "deltaX": delta_x,
                    "deltaY": delta_y,
                }),
            )
            .await
            .log_err();
        })
        .detach();
    }

    /// Forward a key press to the page over CDP. Sends `keyDown` (+ a `char` event carrying `text`
    /// for printable characters, which is what actually inserts the character) then `keyUp`.
    fn forward_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        if self.picking {
            return;
        }
        let SessionState::Connected(cdp) = &self.state else {
            return;
        };
        let ks = &event.keystroke;
        // CDP modifier bitmask: Alt=1, Ctrl=2, Meta/Cmd=4, Shift=8.
        let mut modifiers = 0;
        if ks.modifiers.alt {
            modifiers |= 1;
        }
        if ks.modifiers.control {
            modifiers |= 2;
        }
        if ks.modifiers.platform {
            modifiers |= 4;
        }
        if ks.modifiers.shift {
            modifiers |= 8;
        }
        let (key, code, vk, text) = key_to_cdp(ks);

        let cdp = cdp.clone();
        cx.background_spawn(async move {
            let base = json!({
                "key": key,
                "code": code,
                "windowsVirtualKeyCode": vk,
                "nativeVirtualKeyCode": vk,
                "modifiers": modifiers,
            });
            // keyDown — include `text` so Chrome treats printable keys as character input.
            let mut down = base.clone();
            if let Some(t) = &text {
                down["type"] = json!("keyDown");
                down["text"] = json!(t);
                down["unmodifiedText"] = json!(t);
            } else {
                down["type"] = json!("rawKeyDown");
            }
            cdp.send("Input.dispatchKeyEvent", down).await.log_err();

            let mut up = base;
            up["type"] = json!("keyUp");
            cdp.send("Input.dispatchKeyEvent", up).await.log_err();
        })
        .detach();
    }

    /// Forward mouse movement so the page gets hover states. Fire-and-forget (no pending-map slot)
    /// so a fast drag doesn't accumulate in-flight requests, and only when the cursor actually moved
    /// to a new page coordinate.
    fn forward_move(&mut self, event: &MouseMoveEvent, _cx: &mut Context<Self>) {
        if self.picking {
            return;
        }
        let SessionState::Connected(cdp) = &self.state else {
            return;
        };
        let Some((page_x, page_y)) = self.page_coords(event.position) else {
            return;
        };
        // Skip duplicate positions (GPUI can emit repeats); only forward genuine movement.
        if self.last_page_point == Some((page_x, page_y)) {
            return;
        }
        self.last_page_point = Some((page_x, page_y));

        let buttons = if event.pressed_button == Some(MouseButton::Left) {
            1
        } else {
            0
        };
        cdp.send_no_reply(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseMoved", "x": page_x, "y": page_y, "buttons": buttons }),
        );
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

        let connected = matches!(self.state, SessionState::Connected(_));
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
                    .when(connected, |this| {
                        this.child(
                            IconButton::new("looking-glass-reload", IconName::RotateCw)
                                .tooltip(Tooltip::text("Reload the page"))
                                .on_click(cx.listener(|this, _, _window, cx| this.reload(cx))),
                        )
                    })
                    .child(
                        IconButton::new("looking-glass-pick", IconName::MagnifyingGlass)
                            .tooltip(Tooltip::text(
                                "Pick an element to open its source (⌘⇧I · ⌘K ⌘⇧I from the editor)",
                            ))
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

        // While actively working toward a first frame, show an animated spinner; on a
        // failed/disconnected state show the static product mark.
        let loading = matches!(
            self.state,
            SessionState::Connecting | SessionState::Connected(_)
        );

        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_4()
            .p_8()
            .child(if loading {
                Icon::new(IconName::LoadCircle)
                    .size(IconSize::XLarge)
                    .color(Color::Accent)
                    .with_rotate_animation(3)
                    .into_any_element()
            } else {
                Icon::new(IconName::Screen)
                    .size(IconSize::XLarge)
                    .color(Color::Muted)
                    .into_any_element()
            })
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
            .child(row("⌘⇧I", "Toggle pick mode (panel focused)"))
            .child(row("⌘K ⌘⇧I", "Toggle pick mode from anywhere"))
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

        // Only show the live frame while actually connected. On Disconnected/Failed, fall through to
        // the onboarding view (which carries the Retry button) instead of leaving a frozen, dead
        // last frame on screen with no way to recover.
        let connected = matches!(self.state, SessionState::Connected(_));
        let live_image = if connected {
            self.latest_frame.as_ref().map(|frame| frame.image.clone())
        } else {
            None
        };
        let preview = if let Some(image) = live_image {
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

            let picking = self.picking;
            // Pixel-exact paint (see `pixel_exact_placement`). Falls back to Contain-fit only when
            // the frame doesn't fit the pane (mid-resize, before the new viewport lands) —
            // transiently scaled, never wrongly placed.
            let exact = self.preview_bounds.and_then(|pane| {
                pixel_exact_placement(image.size(0), pane.size, window.scale_factor())
            });
            let frame_image = if let Some(placement) = exact {
                img(image)
                    .absolute()
                    .left(placement.origin.x)
                    .top(placement.origin.y)
                    .w(placement.size.width)
                    .h(placement.size.height)
                    .into_any_element()
            } else {
                img(image)
                    .size_full()
                    .object_fit(ObjectFit::Contain)
                    .into_any_element()
            };
            div()
                    .relative()
                    .size_full()
                    .overflow_hidden()
                    .child(frame_image)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            // Focus the panel so subsequent keystrokes route here, then forward click.
                            window.focus(&this.focus_handle, cx);
                            this.forward_click(event, cx);
                        }),
                    )
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _window, cx| {
                        this.forward_scroll(event, cx);
                    }))
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                        this.forward_move(event, cx);
                    }))
                    // (Key handling lives on the focus-tracked root element — see render's v_flex.)
                    // In-canvas cue so the user knows pick mode is on (and normal clicks are paused).
                    .when(picking, |this| {
                        this.child(
                            div()
                                .absolute()
                                .top_2()
                                .left_0()
                                .right_0()
                                .flex()
                                .justify_center()
                                .child(
                                    div()
                                        .px_2()
                                        .py_1()
                                        .rounded_md()
                                        .bg(colors.elevated_surface_background)
                                        .border_1()
                                        .border_color(colors.border)
                                        .child(
                                            Label::new(
                                                "Pick mode — click an element to open its source · Esc to cancel",
                                            )
                                            .size(LabelSize::Small),
                                        ),
                                ),
                        )
                    })
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

        // Capture, each layout (in BOTH the live and onboarding states, so the first session can
        // launch at the real panel size and resize is detected even with no frames flowing):
        // (1) the image rect where the frame actually paints — the SAME pixel-exact math as the
        // `img` placement above, from the decoded frame's own dimensions — as image_bounds, so
        // click→page mapping is lock-step with the pixels; (2) the panel's fractional bounds and
        // CSS size + scale, to position the next paint and push the viewport to Chrome.
        let frame_px = self
            .latest_frame
            .as_ref()
            .map(|frame| frame.image.size(0));
        let measure = {
            let view = cx.entity().downgrade();
            canvas(
                move |pane_bounds, window, cx| {
                    let scale = window.scale_factor();
                    let image_bounds = frame_px.map(|frame| {
                        match pixel_exact_placement(frame, pane_bounds.size, scale) {
                            Some(placement) => Bounds::new(
                                pane_bounds.origin + placement.origin,
                                placement.size,
                            ),
                            None => ObjectFit::Contain.get_bounds(pane_bounds, frame),
                        }
                    });
                    let css_w = f32::from(pane_bounds.size.width) as u32;
                    let css_h = f32::from(pane_bounds.size.height) as u32;
                    view.update(cx, |this, cx| {
                        this.image_bounds = image_bounds;
                        this.preview_bounds = Some(pane_bounds);
                        this.sync_viewport(css_w, css_h, scale, cx);
                    })
                    .ok();
                },
                |_bounds, _, _window, _cx| {},
            )
            .absolute()
            // Explicit insets are load-bearing: an absolutely-positioned element WITHOUT them is
            // laid out at its *static position* (below/after its sibling), not at the parent's
            // origin. This canvas then reported a rect hanging past the pane's bottom edge, so
            // every click was "outside page area" and the garbage size was even pushed to Chrome
            // as the viewport (observed live: image_bounds origin y=745 in a pane starting at
            // y=75, with all clicks ignored).
            .inset_0()
            .size_full()
        };

        v_flex()
            .key_context("WebPreview")
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(colors.editor_background)
            // Key events route along the FOCUS path, and only this element tracks focus — so the
            // key handler must live here (not on the inner preview div, which never gets focus) for
            // typing into the previewed page to work.
            .on_key_down(
                cx.listener(|this, event: &gpui::KeyDownEvent, _window, cx| {
                    this.forward_key(event, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &ToggleWebPickMode, _window, cx| this.toggle_pick_mode(cx)),
            )
            .on_action(cx.listener(|this, _: &ReloadWebPreview, _window, cx| this.reload(cx)))
            .on_action(cx.listener(|this, _: &menu::Cancel, _window, cx| this.cancel_pick_mode(cx)))
            .child(header)
            .child(
                h_flex()
                    .size_full()
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_hidden()
                            .child(preview)
                            .child(measure),
                    )
                    .child(
                        div()
                            .flex_none()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_metrics_clamp_css_dimensions_and_track_scale() {
        assert_eq!(viewport_metrics(0, 0, 0.5), (1, 1, 1000));
        assert_eq!(viewport_metrics(400, 300, 2.0), (400, 300, 2000));
        assert_eq!(viewport_metrics(400, 300, 1.25), (400, 300, 1250));
    }

    #[test]
    fn screencast_bounds_request_physical_frame_bounds_without_changing_css_viewport() {
        assert_eq!(screencast_bounds(400, 300, 2.0), (800, 600));
        assert_eq!(screencast_bounds(400, 300, 1.25), (500, 375));
        assert_eq!(screencast_bounds(0, 0, 0.5), (1, 1));
    }

    #[test]
    fn space_key_carries_text_so_chrome_inserts_a_space() {
        // A `rawKeyDown` without `text` inserts nothing in Chrome; space must go down the
        // printable-key path with text " ".
        let keystroke = gpui::Keystroke {
            modifiers: gpui::Modifiers::default(),
            key: "space".to_string(),
            key_char: None,
        };
        let (key, code, vk, text) = key_to_cdp(&keystroke);
        assert_eq!(key, " ");
        assert_eq!(code, "Space");
        assert_eq!(vk, 32);
        assert_eq!(text.as_deref(), Some(" "));
    }

    #[test]
    fn pixel_exact_placement_is_one_to_one_with_device_pixels() {
        // 2560x1600 frame on a 2x window must paint at exactly 1280x800 logical px so the snapped
        // quad equals the texture size (1:1 sampling, no bilinear blur).
        let placement = pixel_exact_placement(
            gpui::size(gpui::DevicePixels(2560), gpui::DevicePixels(1600)),
            gpui::size(px(1280.7), px(800.3)),
            2.0,
        )
        .expect("frame fits");
        assert_eq!(f32::from(placement.size.width), 1280.0);
        assert_eq!(f32::from(placement.size.height), 800.0);
        // Centered within the fractional pane.
        assert!((f32::from(placement.origin.x) - 0.35).abs() < 0.01);
        assert!((f32::from(placement.origin.y) - 0.15).abs() < 0.01);
    }

    #[test]
    fn pixel_exact_placement_tolerates_chrome_rounding_at_fractional_scale() {
        // At scale 1.25 a 333-css-px pane captures round(333*1.25)=416 device px; 416/1.25=332.8
        // logical px must still take the exact path (sharp), not fall back to a scaled fit.
        let placement = pixel_exact_placement(
            gpui::size(gpui::DevicePixels(416), gpui::DevicePixels(416)),
            gpui::size(px(333.0), px(333.0)),
            1.25,
        );
        assert!(placement.is_some());
    }

    #[test]
    fn pixel_exact_placement_rejects_oversized_frames() {
        // A stale 2x-of-large-pane frame during a shrink must fall back to scaled fit, not overflow.
        let placement = pixel_exact_placement(
            gpui::size(gpui::DevicePixels(2560), gpui::DevicePixels(1600)),
            gpui::size(px(600.0), px(400.0)),
            2.0,
        );
        assert!(placement.is_none());
    }

    #[test]
    fn named_control_keys_have_no_text() {
        let keystroke = gpui::Keystroke {
            modifiers: gpui::Modifiers::default(),
            key: "enter".to_string(),
            key_char: None,
        };
        let (key, _, vk, text) = key_to_cdp(&keystroke);
        assert_eq!(key, "Enter");
        assert_eq!(vk, 13);
        assert_eq!(text, None);
    }
}

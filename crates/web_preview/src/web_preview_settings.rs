use crate::source_map::Framework;
use settings::{RegisterSetting, Settings};

/// Default dev-server URL when unset. Single source of truth — also referenced in the settings
/// content doc comment and `script/check-web-preview-env`.
pub const DEFAULT_URL: &str = "http://localhost:5173";

/// Default Chrome remote-debugging port. A value equal to this is treated as "auto": the session
/// allocates a free port instead, so two panels never collide. Any other value is an explicit
/// override that is used verbatim.
pub const DEFAULT_REMOTE_DEBUGGING_PORT: u16 = 9222;

/// Default dev-server readiness timeout, in seconds.
pub const DEFAULT_DEV_SERVER_TIMEOUT_SECS: u64 = 30;

/// Resolved settings for the in-editor web app preview.
#[derive(Clone, Debug, RegisterSetting)]
pub struct WebPreviewSettings {
    /// URL of the running dev server to preview.
    pub url: String,
    /// Explicit path to the Chrome/Chromium executable, if configured.
    pub chrome_path: Option<String>,
    /// Port Chrome exposes its remote debugging endpoint on.
    pub remote_debugging_port: u16,
    /// Command to launch the dev server if the URL isn't reachable.
    pub dev_command: Option<String>,
    /// An explicit framework override, or `None` for auto-detect.
    pub framework_override: Option<Framework>,
    /// How long to wait for the dev server to become reachable.
    pub dev_server_timeout_secs: u64,
    /// Screencast JPEG quality (1–100).
    pub screencast_quality: u8,
    /// Render only every Nth streamed frame (client-side throttle on top of the built-in ~30 fps
    /// cap). Never passed to Chrome's `everyNthFrame`, which would drop the single frame a static
    /// page emits and blank the preview.
    pub screencast_every_nth_frame: u32,
}

fn parse_framework(value: &str) -> Option<Framework> {
    match value.trim().to_lowercase().as_str() {
        "svelte" => Some(Framework::Svelte),
        "react" => Some(Framework::React),
        "vue" => Some(Framework::Vue),
        _ => None, // "auto" or anything unrecognized → auto-detect.
    }
}

impl Settings for WebPreviewSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let web_preview = content.web_preview.as_ref();
        WebPreviewSettings {
            url: web_preview
                .and_then(|settings| settings.url.clone())
                .unwrap_or_else(|| DEFAULT_URL.to_string()),
            chrome_path: web_preview.and_then(|settings| settings.chrome_path.clone()),
            remote_debugging_port: web_preview
                .and_then(|settings| settings.remote_debugging_port)
                .unwrap_or(DEFAULT_REMOTE_DEBUGGING_PORT),
            dev_command: web_preview.and_then(|settings| settings.dev_command.clone()),
            framework_override: web_preview
                .and_then(|settings| settings.framework.as_deref())
                .and_then(parse_framework),
            dev_server_timeout_secs: web_preview
                .and_then(|settings| settings.dev_server_timeout_secs)
                .unwrap_or(DEFAULT_DEV_SERVER_TIMEOUT_SECS),
            screencast_quality: web_preview
                .and_then(|settings| settings.screencast_quality)
                .unwrap_or(90)
                .clamp(1, 100) as u8,
            screencast_every_nth_frame: web_preview
                .and_then(|settings| settings.screencast_every_nth_frame)
                .unwrap_or(1)
                .max(1),
        }
    }
}

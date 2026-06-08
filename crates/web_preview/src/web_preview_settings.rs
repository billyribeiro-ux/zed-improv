use settings::{RegisterSetting, Settings};

/// Default dev-server URL when unset. Single source of truth — also referenced in the settings
/// content doc comment and `script/check-web-preview-env`.
pub const DEFAULT_URL: &str = "http://localhost:5173";

/// Default Chrome remote-debugging port. A value equal to this is treated as "auto": the session
/// allocates a free port instead, so two panels never collide. Any other value is an explicit
/// override that is used verbatim.
pub const DEFAULT_REMOTE_DEBUGGING_PORT: u16 = 9222;

/// Resolved settings for the in-editor web app preview.
#[derive(Clone, Debug, RegisterSetting)]
pub struct WebPreviewSettings {
    /// URL of the running dev server to preview.
    pub url: String,
    /// Explicit path to the Chrome/Chromium executable, if configured.
    pub chrome_path: Option<String>,
    /// Port Chrome exposes its remote debugging endpoint on.
    pub remote_debugging_port: u16,
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
        }
    }
}

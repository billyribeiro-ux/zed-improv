use settings::{RegisterSetting, Settings};

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
                .unwrap_or_else(|| "http://localhost:5173".to_string()),
            chrome_path: web_preview.and_then(|settings| settings.chrome_path.clone()),
            remote_debugging_port: web_preview
                .and_then(|settings| settings.remote_debugging_port)
                .unwrap_or(9222),
        }
    }
}

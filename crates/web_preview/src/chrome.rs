//! Locate, launch, and discover an external Chrome/Chromium instance exposing the Chrome DevTools
//! Protocol over `--remote-debugging-port`.
//!
//! We deliberately drive an *external* browser rather than embedding a webview: native webviews
//! (WKWebView/WebView2/WebKitGTK, e.g. via `wry`) do not speak CDP, which is what powers element
//! inspection and live CSS editing. Chrome is the user's already-installed browser, launched as a
//! child process (the same model `crates/terminal` uses to spawn a dev server).

use anyhow::{Context as _, Result, bail};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// A handle to a launched Chrome process. Dropping it kills the child so we don't leak browsers.
pub struct ChromeProcess {
    child: smol::process::Child,
    /// The page target's `webSocketDebuggerUrl`, used by [`crate::cdp::CdpClient::connect`].
    pub ws_url: String,
}

impl Drop for ChromeProcess {
    fn drop(&mut self) {
        // Best-effort: the OS reaps the child once killed.
        let _ = self.child.kill();
    }
}

/// Candidate executable names/paths to search, in priority order, when no explicit path is set.
#[cfg(target_os = "macos")]
const CHROME_CANDIDATES: &[&str] = &[
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "google-chrome",
    "chromium",
];

#[cfg(target_os = "linux")]
const CHROME_CANDIDATES: &[&str] = &[
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
];

#[cfg(target_os = "windows")]
const CHROME_CANDIDATES: &[&str] = &[
    "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
    "C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe",
    "chrome",
    "chromium",
];

/// Resolve the Chrome/Chromium executable, honoring an explicit override first.
pub fn locate_chrome(explicit_path: Option<&str>) -> Result<PathBuf> {
    if let Some(path) = explicit_path {
        let path = PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
        bail!("configured chrome_path does not exist: {}", path.display());
    }

    for candidate in CHROME_CANDIDATES {
        let candidate_path = PathBuf::from(candidate);
        if candidate_path.is_absolute() && candidate_path.exists() {
            return Ok(candidate_path);
        }
        if let Ok(found) = which::which(candidate) {
            return Ok(found);
        }
    }

    bail!(
        "could not find Chrome or Chromium. Install it, or set `web_preview.chrome_path` in settings."
    )
}

/// Launch Chrome at `url` with remote debugging enabled, then discover the page's CDP websocket URL.
///
/// `user_data_dir` isolates this browser session from the user's normal profile (required for
/// remote debugging to attach reliably). `port` of 0 lets Chrome pick a free port, which we then
/// read back from the DevToolsActivePort file is not used here — instead we require a fixed port so
/// `/json/list` is reachable at a known address.
pub async fn launch(
    chrome_path: PathBuf,
    url: &str,
    port: u16,
    user_data_dir: PathBuf,
    http_client: Arc<dyn HttpClient>,
    executor: gpui::BackgroundExecutor,
) -> Result<ChromeProcess> {
    let child = smol::process::Command::new(&chrome_path)
        .arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", user_data_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg(url)
        .spawn()
        .with_context(|| format!("spawning chrome at {}", chrome_path.display()))?;

    // Poll /json/list until Chrome's debugging endpoint is up and a "page" target exists.
    let ws_url = discover_page_ws_url(port, &http_client, &executor)
        .await
        .context("discovering CDP page target")?;

    Ok(ChromeProcess { child, ws_url })
}

#[derive(Debug, Deserialize)]
struct CdpTarget {
    #[serde(rename = "type")]
    target_type: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    web_socket_debugger_url: Option<String>,
}

async fn discover_page_ws_url(
    port: u16,
    http_client: &Arc<dyn HttpClient>,
    executor: &gpui::BackgroundExecutor,
) -> Result<String> {
    let endpoint = format!("http://127.0.0.1:{port}/json/list");
    let mut last_error = None;

    // Chrome can take a moment to open the debugging port; retry for a few seconds.
    for _ in 0..50 {
        match fetch_targets(&endpoint, http_client).await {
            Ok(targets) => {
                if let Some(ws_url) = targets
                    .into_iter()
                    .find(|target| target.target_type == "page")
                    .and_then(|target| target.web_socket_debugger_url)
                {
                    return Ok(ws_url);
                }
            }
            Err(error) => last_error = Some(error),
        }
        executor.timer(Duration::from_millis(100)).await;
    }

    match last_error {
        Some(error) => Err(error).context("chrome debugging endpoint never became ready"),
        None => bail!("chrome started but exposed no \"page\" target with a debugger URL"),
    }
}

async fn fetch_targets(
    endpoint: &str,
    http_client: &Arc<dyn HttpClient>,
) -> Result<Vec<CdpTarget>> {
    let mut response = http_client.get(endpoint, AsyncBody::empty(), false).await?;
    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;
    let targets = serde_json::from_slice(&body).context("parsing /json/list response")?;
    Ok(targets)
}

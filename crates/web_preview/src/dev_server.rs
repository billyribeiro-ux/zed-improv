//! Detect (and optionally launch) the user's web dev server.
//!
//! v1 keeps this intentionally small: the user runs their dev server however they like and we just
//! need its URL to be reachable before we point Chrome at it. Launching the dev command from Zed
//! (reusing the task/terminal infrastructure) is a follow-up; for now we poll the configured URL.

use anyhow::{Context as _, Result, bail};
use http_client::{AsyncBody, HttpClient};
use std::sync::Arc;
use std::time::Duration;

/// Poll `url` until it responds, so we don't navigate Chrome to a dev server that isn't up yet.
/// Gives up after roughly `timeout`.
pub async fn wait_until_ready(
    url: &str,
    http_client: Arc<dyn HttpClient>,
    executor: gpui::BackgroundExecutor,
    timeout: Duration,
) -> Result<()> {
    let deadline_ticks = (timeout.as_millis() / 200).max(1);
    let mut last_error = None;

    for _ in 0..deadline_ticks {
        match http_client.get(url, AsyncBody::empty(), true).await {
            // Any HTTP response (even a 404) means the server is listening.
            Ok(_) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        executor.timer(Duration::from_millis(200)).await;
    }

    match last_error {
        Some(error) => Err(error).with_context(|| format!("dev server at {url} never responded")),
        None => bail!("dev server at {url} never responded"),
    }
}

//! Detect (and optionally launch) the user's web dev server.
//!
//! The user can either run the dev server themselves (we just poll the URL until it responds) or set
//! `web_preview.dev_command`, in which case we launch it in the project root when the URL isn't
//! already reachable.

use anyhow::{Context as _, Result, bail};
use gpui::{AsyncApp, Entity};
use http_client::{AsyncBody, HttpClient};
use project::Project;
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

/// Whether `url` already responds (used to avoid launching a dev server that's already running).
pub async fn is_reachable(url: &str, http_client: Arc<dyn HttpClient>) -> bool {
    http_client.get(url, AsyncBody::empty(), true).await.is_ok()
}

/// Launch `command` in the project's first visible worktree root as a detached child process. The
/// child runs independently (it is *not* killed when the preview closes — the user may want their
/// dev server to keep running). Best-effort: failures are logged, not propagated.
pub async fn spawn_dev_command(command: &str, project: &Entity<Project>, cx: &mut AsyncApp) {
    let cwd = project.read_with(cx, |project, cx| {
        project
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
    });
    let Some(cwd) = cwd else {
        log::warn!("web preview: no worktree to run dev_command in");
        return;
    };

    // Run through a shell so commands like `npm run dev` work as written.
    let (shell, flag) = if cfg!(target_os = "windows") {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };

    match smol::process::Command::new(shell)
        .arg(flag)
        .arg(command)
        .current_dir(&cwd)
        .spawn()
    {
        Ok(child) => {
            // Let it run independently: hold the handle in a detached task that simply awaits the
            // child, so smol doesn't reap it and it keeps running for the session's lifetime.
            cx.background_executor()
                .spawn(async move {
                    let mut child = child;
                    let _ = child.status().await;
                })
                .detach();
        }
        Err(error) => {
            log::warn!("web preview: failed to launch dev_command '{command}': {error:#}");
        }
    }
}

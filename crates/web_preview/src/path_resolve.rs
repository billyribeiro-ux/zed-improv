//! Resolve a path reported by the browser (a framework `__source` file, a Vue `data-v-inspector`
//! attribute, or a stylesheet `sourceURL`) to an absolute path inside one of the project's
//! worktrees. Shared by click-to-source ([`crate::web_preview`]) and CSS write-back
//! ([`crate::css_panel`]) so the two never diverge.
//!
//! Handles the path shapes a dev server actually emits:
//! - **Absolute** paths (`/Users/me/app/src/Foo.jsx`) — used directly when they exist.
//! - **`file://`** URLs — the remainder is an absolute path.
//! - Vite **`/@fs/<abs>`** URLs — the remainder is an absolute path.
//! - **`http(s)://host[:port]/<path>`** — strip the scheme + host, keep the path.
//! - A `?query` / `#fragment` (Vite transform markers) — stripped.
//! - **Project-relative** paths, including monorepo / `src/`-prefix skew — matched by trying the
//!   path tail against each worktree, trimming leading segments until one resolves on disk.

use project::Project;
use std::path::{Path, PathBuf};

/// Resolve `reported` (a URL or path from the browser) to an existing absolute file in a worktree.
pub fn resolve(project: &gpui::Entity<Project>, reported: &str, cx: &gpui::App) -> Option<PathBuf> {
    let path = strip_to_path(reported);

    // 1. Absolute path that exists — use directly (covers file://, /@fs/, and absolute __source).
    let absolute = Path::new(&path);
    if absolute.is_absolute() && absolute.exists() {
        return Some(absolute.to_path_buf());
    }

    let relative = path.trim_start_matches('/');
    let worktrees: Vec<PathBuf> = project
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .collect();

    // 2. Direct join onto each worktree root.
    for root in &worktrees {
        let candidate = root.join(relative);
        if candidate.exists() {
            return Some(candidate);
        }
    }

    // 3. Tail matching: trim leading segments of the reported path (e.g. monorepo `apps/web/src/…`
    //    vs a worktree rooted at `apps/web`, or vice versa) and retry against each worktree.
    let segments: Vec<&str> = relative.split('/').filter(|s| !s.is_empty()).collect();
    for start in 1..segments.len() {
        let tail = segments[start..].join("/");
        for root in &worktrees {
            let candidate = root.join(&tail);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }

    None
}

/// Strip a URL down to its path component (or return the input unchanged if it isn't a URL).
fn strip_to_path(reported: &str) -> String {
    // Drop any query string / fragment (Vite appends `?t=…`, `?import`, etc.).
    let without_query = reported.split(['?', '#']).next().unwrap_or(reported);

    if let Some(rest) = without_query.strip_prefix("file://") {
        // file:///Users/... -> /Users/...
        return rest.to_string();
    }
    if let Some(rest) = without_query.strip_prefix("/@fs/") {
        // Vite serves out-of-root files at /@fs/<absolute path>.
        return format!("/{}", rest.trim_start_matches('/'));
    }
    if let Some((_scheme, rest)) = without_query.split_once("://") {
        // http://host:port/path -> /path  (drop the host segment).
        return match rest.split_once('/') {
            Some((_host, path)) => format!("/{path}"),
            None => "/".to_string(),
        };
    }
    without_query.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_http_host_and_query() {
        assert_eq!(
            strip_to_path("http://localhost:5173/src/app.css?t=123"),
            "/src/app.css"
        );
    }

    #[test]
    fn strips_file_scheme() {
        assert_eq!(
            strip_to_path("file:///Users/me/app/src/Foo.svelte"),
            "/Users/me/app/src/Foo.svelte"
        );
    }

    #[test]
    fn strips_vite_fs_prefix() {
        assert_eq!(
            strip_to_path("/@fs/Users/me/app/node_modules/x/y.css"),
            "/Users/me/app/node_modules/x/y.css"
        );
    }

    #[test]
    fn leaves_plain_relative_path() {
        assert_eq!(
            strip_to_path("src/routes/+page.svelte"),
            "src/routes/+page.svelte"
        );
    }

    #[test]
    fn strips_query_from_relative() {
        assert_eq!(strip_to_path("src/app.css?import"), "src/app.css");
    }
}

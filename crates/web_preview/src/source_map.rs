//! Map a clicked DOM element back to the source location that rendered it.
//!
//! Unlike Cursor — which forwards the selected element to an AI agent that *infers* the file — we
//! resolve the exact `file:line:column` deterministically, using the framework's own dev-mode
//! source metadata.
//!
//! Supported frameworks:
//! - **Svelte** via the compiler's native `__svelte_meta.loc` property (a *JavaScript property* on
//!   the node, read with `Runtime.callFunctionOn`). Note: `loc` is emitted *after* preprocessors
//!   run and is not mapped back through their sourcemaps, so for `<script lang="ts">`, MDsveX, or
//!   markup-level PostCSS the line can be offset from the on-disk file. Exact for unpreprocessed
//!   components, approximate for preprocessed ones.
//! - **React** via the fiber `__source` / `_debugSource`. Best-effort: stock Vite + React 19 no
//!   longer emits it, so this can fall back to selector-only with a hint.
//! - **Vue** via the `data-v-inspector` attribute (`vite-plugin-vue-inspector`).

use crate::cdp::CdpClient;
use anyhow::{Context as _, Result};
use serde_json::{Value, json};

/// The detected client framework, which selects the source resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framework {
    Svelte,
    React,
    Vue,
    Unknown,
}

impl Framework {
    /// Short label for the connection status line.
    pub fn label(self) -> &'static str {
        match self {
            Framework::Svelte => "Svelte",
            Framework::React => "React",
            Framework::Vue => "Vue",
            Framework::Unknown => "unknown framework",
        }
    }
}

/// A resolved source location for a picked element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    /// Path as reported by the framework, relative to the project root (e.g. `src/routes/+page.svelte`).
    pub file: String,
    /// 1-based line, as reported by the framework. Convert to 0-based before building a `Point`.
    pub line: u32,
    /// 1-based column, as reported by the framework.
    pub column: u32,
}

/// The outcome of resolving a picked element to source. When deterministic mapping is unavailable
/// we degrade to a CSS selector rather than failing outright, so live CSS still works.
#[derive(Debug, Clone)]
pub enum Resolution {
    /// Deterministic source location.
    Source(SourceLocation),
    /// No source metadata found; a stable selector for the element (selector-only mode). `hint`, if
    /// present, explains *why* mapping failed and how to enable it (e.g. a missing dev plugin).
    SelectorOnly {
        selector: String,
        hint: Option<String>,
    },
}

/// Detect the client framework by probing for runtime markers.
///
/// We pick the framework whose *dev source metadata* we can actually read, in priority order:
/// Svelte (`__svelte_meta` on nodes), then Vue (`data-v-inspector` from `vite-plugin-vue-inspector`),
/// then React (a `__reactFiber$*` key on nodes or the React DevTools global hook).
///
/// Cost-bounded: it checks cheap global markers first, then scans at most `MAX_SCAN` elements (a
/// no-marker page — not-yet-hydrated, prod build, plain HTML — must not block the page's main thread
/// enumerating every node's inherited keys). Because detection depends on hydration having run, it
/// may return `Unknown` if probed too early; callers should re-probe on `Unknown` (see
/// [`detect_framework_if_unknown`]).
pub async fn detect_framework(cdp: &CdpClient) -> Framework {
    let expression = r#"
        (() => {
            const MAX_SCAN = 2000;
            // Cheap global markers first.
            if (window.__svelte) {
                // window.__svelte exists for Svelte 5 but doesn't confirm dev source metadata.
            }
            if (document.querySelector('[data-v-inspector]')) return 'vue';
            const all = document.querySelectorAll('*');
            const limit = Math.min(all.length, MAX_SCAN);
            let sawReact = !!window.__REACT_DEVTOOLS_GLOBAL_HOOK__;
            for (let i = 0; i < limit; i++) {
                const el = all[i];
                if (el.__svelte_meta) return 'svelte';
                if (!sawReact) {
                    for (const key in el) {
                        if (key.startsWith('__reactFiber$')) { sawReact = true; break; }
                    }
                }
            }
            if (sawReact) return 'react';
            return 'unknown';
        })()
    "#;

    match evaluate_string(cdp, expression).await.as_deref() {
        Some("svelte") => Framework::Svelte,
        Some("vue") => Framework::Vue,
        Some("react") => Framework::React,
        _ => Framework::Unknown,
    }
}

/// Re-run detection only if the current framework is `Unknown`. Used at pick time to recover from an
/// early probe that ran before the app hydrated, or after navigating to a differently-built route.
pub async fn detect_framework_if_unknown(cdp: &CdpClient, current: Framework) -> Framework {
    if current == Framework::Unknown {
        detect_framework(cdp).await
    } else {
        current
    }
}

/// Resolve a picked node (identified by its `Runtime.RemoteObject` `objectId`) to source.
pub async fn resolve(cdp: &CdpClient, framework: Framework, object_id: &str) -> Result<Resolution> {
    let function = match framework {
        Framework::Svelte => SVELTE_RESOLVER,
        Framework::React => REACT_RESOLVER,
        Framework::Vue => VUE_RESOLVER,
        Framework::Unknown => {
            return Ok(Resolution::SelectorOnly {
                selector: selector_for(cdp, object_id).await.unwrap_or_default(),
                hint: None,
            });
        }
    };

    let result = call_function_on(cdp, object_id, function).await?;
    if let Some(location) = parse_location(&result) {
        return Ok(Resolution::Source(location));
    }

    // Detected the framework but found no source metadata — give a framework-specific reason.
    Ok(Resolution::SelectorOnly {
        selector: selector_for(cdp, object_id).await.unwrap_or_default(),
        hint: Some(missing_metadata_hint(framework)),
    })
}

/// Explain why a detected framework yielded no source metadata, and how to enable it.
fn missing_metadata_hint(framework: Framework) -> String {
    match framework {
        Framework::React => "No React source metadata for this element. Stock Vite + React 19 \
            doesn't emit it — add a click-to-component setup or the classic JSX-source transform."
            .to_string(),
        Framework::Vue => "No Vue source metadata. Enable `vite-plugin-vue-inspector` in your dev \
            config to map elements to `.vue` files."
            .to_string(),
        Framework::Svelte => {
            "No Svelte source metadata for this element (it may be a non-component \
            node, or built without dev metadata)."
                .to_string()
        }
        Framework::Unknown => String::new(),
    }
}

/// Reads Svelte's native `__svelte_meta.loc`, walking ancestors until one carries it.
const SVELTE_RESOLVER: &str = r#"
    function() {
        let node = this;
        while (node) {
            const meta = node.__svelte_meta;
            if (meta && meta.loc) {
                return { file: meta.loc.file, line: meta.loc.line, column: meta.loc.column };
            }
            node = node.parentElement;
        }
        return null;
    }
"#;

/// Reads React's `__source` ({fileName, lineNumber, columnNumber}) from the element's fiber, then
/// from the owner chain. `__source` requires `@babel/plugin-transform-react-jsx-source` (Vite's
/// React dev mode enables it). React 19 removed `_debugSource` from fibers, so we read `__source`
/// off `memoizedProps`/`pendingProps` and walk `_debugOwner`/`return` as fallbacks.
const REACT_RESOLVER: &str = r#"
    function() {
        function toLoc(src) {
            if (!src || !src.fileName) return null;
            return { file: src.fileName, line: src.lineNumber || 1, column: (src.columnNumber || 1) };
        }
        function fiberOf(node) {
            for (const key in node) {
                if (key.startsWith('__reactFiber$') || key.startsWith('__reactInternalInstance$')) {
                    return node[key];
                }
            }
            return null;
        }
        let node = this;
        while (node) {
            let fiber = fiberOf(node);
            let hops = 0;
            while (fiber && hops < 50) {
                const src =
                    (fiber.memoizedProps && fiber.memoizedProps.__source) ||
                    (fiber.pendingProps && fiber.pendingProps.__source) ||
                    fiber._debugSource;
                const loc = toLoc(src);
                if (loc) return loc;
                fiber = fiber._debugOwner || fiber.return;
                hops++;
            }
            node = node.parentElement;
        }
        return null;
    }
"#;

/// Reads Vue's `data-v-inspector="file:line:column"` attribute injected by
/// `vite-plugin-vue-inspector`, walking ancestors until one carries it.
const VUE_RESOLVER: &str = r#"
    function() {
        let node = this;
        while (node) {
            if (node.getAttribute) {
                const value = node.getAttribute('data-v-inspector');
                if (value) {
                    const parts = value.split(':');
                    if (parts.length >= 3) {
                        const column = parseInt(parts[parts.length - 1], 10);
                        const line = parseInt(parts[parts.length - 2], 10);
                        const file = parts.slice(0, parts.length - 2).join(':');
                        return { file: file, line: line, column: column };
                    }
                }
            }
            node = node.parentElement;
        }
        return null;
    }
"#;

fn parse_location(value: &Value) -> Option<SourceLocation> {
    let file = value.get("file")?.as_str()?.to_string();
    let line = value.get("line")?.as_u64()? as u32;
    // Framework line numbers are 1-based. A line of 0 violates that invariant (e.g. an unguarded
    // `parseInt` returning 0); reject it so we degrade to selector-only rather than opening row 0.
    if line == 0 {
        return None;
    }
    // Column is optional; default to 0 when absent rather than failing the whole parse.
    let column = value.get("column").and_then(Value::as_u64).unwrap_or(0) as u32;
    Some(SourceLocation { file, line, column })
}

/// Build a reasonably stable CSS selector for an element, used in selector-only mode so the CSS
/// panel can still target it.
async fn selector_for(cdp: &CdpClient, object_id: &str) -> Result<String> {
    let function = r#"
        function() {
            if (this.id) return '#' + this.id;
            const parts = [];
            let node = this;
            while (node && node.nodeType === 1 && parts.length < 5) {
                let part = node.tagName.toLowerCase();
                if (node.classList && node.classList.length) {
                    part += '.' + Array.from(node.classList).join('.');
                }
                const parent = node.parentElement;
                if (parent) {
                    const index = Array.from(parent.children).indexOf(node) + 1;
                    part += ':nth-child(' + index + ')';
                }
                parts.unshift(part);
                node = node.parentElement;
            }
            return parts.join(' > ');
        }
    "#;
    let result = call_function_on(cdp, object_id, function).await?;
    Ok(result.as_str().unwrap_or_default().to_string())
}

async fn call_function_on(cdp: &CdpClient, object_id: &str, function: &str) -> Result<Value> {
    let response = cdp
        .send(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": function,
                "returnByValue": true,
            }),
        )
        .await
        .context("Runtime.callFunctionOn")?;

    Ok(response
        .get("result")
        .and_then(|result| result.get("value"))
        .cloned()
        .unwrap_or(Value::Null))
}

async fn evaluate_string(cdp: &CdpClient, expression: &str) -> Option<String> {
    let response = cdp
        .send(
            "Runtime.evaluate",
            json!({ "expression": expression, "returnByValue": true }),
        )
        .await
        .ok()?;

    response
        .get("result")
        .and_then(|result| result.get("value"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_svelte_meta_location() {
        // Shape returned by the `__svelte_meta.loc` reader function.
        let value = json!({ "file": "src/routes/+page.svelte", "line": 18, "column": 4 });
        let location = parse_location(&value).expect("should parse");
        assert_eq!(location.file, "src/routes/+page.svelte");
        assert_eq!(location.line, 18);
        assert_eq!(location.column, 4);
    }

    #[test]
    fn missing_column_defaults_to_zero() {
        let value = json!({ "file": "src/App.svelte", "line": 3 });
        let location = parse_location(&value).expect("should parse");
        assert_eq!(location.column, 0);
    }

    #[test]
    fn null_result_yields_none() {
        assert!(parse_location(&Value::Null).is_none());
    }

    #[test]
    fn line_zero_is_rejected() {
        // A 0 line violates the 1-based invariant and must degrade to selector-only, not open row 0.
        let value = json!({ "file": "src/App.svelte", "line": 0, "column": 4 });
        assert!(parse_location(&value).is_none());
    }
}

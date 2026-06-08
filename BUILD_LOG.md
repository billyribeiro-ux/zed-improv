# Build Log

## 2026-06-07 — `web_preview` crate (in-editor web app preview + deterministic click-to-source + live CSS)

**Files touched:**
- `crates/web_preview/` (new crate): `Cargo.toml`, `src/web_preview.rs` (root + `WebPreviewView` panel),
  `src/cdp.rs`, `src/chrome.rs`, `src/dev_server.rs`, `src/source_map.rs`, `src/screencast.rs`,
  `src/css_panel.rs`, `src/web_preview_settings.rs`, `MANUAL_TESTING.md`
- `Cargo.toml` (workspace members + dependency)
- `crates/zed/Cargo.toml` (+ `web_preview` dep), `crates/zed/src/main.rs` (`web_preview::init`)
- `crates/settings_content/src/settings_content.rs` (`WebPreviewSettingsContent` + `web_preview` field)
- `crates/settings/src/vscode_import.rs` (explicit `SettingsContent` construction needed the new field)

**What it does:** Previews the user's running web app *inside* Zed (Chrome driven over the Chrome
DevTools Protocol, page streamed into a panel via screencast). Pick mode resolves a clicked DOM
element to its exact source `file:line:column` and opens it. A side panel edits the element's inline
CSS live. v1 targets **Svelte**.

**Key decisions (and why):**
- **Deterministic, not agent-guessed.** Research confirmed Cursor's "Select element" feeds the node
  to its AI agent, which *infers* the file; true inspect→open-exact-line is an unfulfilled Cursor
  feature request. We resolve source deterministically from framework dev metadata — surpassing
  Cursor on this axis.
- **Svelte first via native `__svelte_meta.loc`.** The Svelte compiler attaches
  `{ file, line, column, char }` to every DOM node in dev mode — zero plugins required, and it
  matches the operator's own SvelteKit stack. Read as a **JS property** (not an HTML attribute) via
  CDP `Runtime.callFunctionOn`. React (fiber `__source`) and Vue (`data-v-inspector`) deferred —
  React is the highest risk (fiber field names vary; React 19 removed `_debugSource`).
- **External Chrome over CDP, not an embedded webview.** Native webviews (`wry`/WKWebView/WebView2)
  do not speak CDP, which is what powers inspect/highlight/live-CSS. Embedding CEF was rejected on
  binary size and event-loop conflict with GPUI's platform layer.
- **Hand-rolled CDP client** over `async-tungstenite` + `gpui_tokio` (the proven `repl`/`client`
  pattern), rather than `chromiumoxide`/`headless_chrome` — those pull a second Tokio runtime and
  large generated bindings for the ~15 methods we use.
- **Embedded preview via the cross-platform `img(RenderImage)` path** from
  `livekit_client::remote_video_track_view` (NOT `gpui::surface`, which is macOS-only) — decode CDP
  screencast JPEG → BGRA `RenderImage`, forward clicks via `Input.dispatchMouseEvent`.

**Gotchas logged:**
- `gpui::surface` is macOS-only (`CVPixelBuffer`); the cross-platform external-frame primitive is
  `img(RenderImage)` + explicit `window.drop_image` lifecycle.
- `Editor::for_buffer` requires a real `&mut Window` — can't build an editor inside a windowless
  `cx.spawn`. `CssPanel` builds its editor once in `new()` and resets buffer text on each pick.
- Background→entity frame routing must use `cx.spawn` (yields an `AsyncApp` that can `this.update`),
  not `cx.background_spawn` alone (no entity access). Modeled on the livekit frame loop.
- For async open-source the view stores an `AnyWindowHandle` and uses `window_handle.update(cx, …)`,
  since `open_abs_path`/`go_to_singleton_buffer_point_silently` need a `Window`.
- A unit test caught a real bug: `parse_location` used `value.get("column")?` which short-circuited
  the whole parse when `column` was absent instead of defaulting to 0.

**Verification:** `web_preview` compiles with zero errors/warnings; `clippy -D warnings` clean;
`cargo fmt --check` clean; 7/7 unit tests pass (coordinate mapping + source-location parsing).
Full-app build (and live runtime test) blocked in this environment by the missing Metal toolchain
(`xcrun metal`), unrelated to this code — see `crates/web_preview/MANUAL_TESTING.md`.

**Extension points:** `source_map.rs` `Framework` enum + per-framework resolver (add React/Vue);
`css_panel.rs` (matched-rule editing + write-back to source; agent-applied fallback for
scoped/CSS-in-JS); `screencast.rs` (HiDPI coordinate accuracy).

## 2026-06-07 — `web_preview` v2/v3 (CSS write-back to source + React/Vue resolvers)

**Files touched:** `crates/web_preview/src/source_map.rs`, `crates/web_preview/src/css_panel.rs`,
`crates/web_preview/src/web_preview.rs`, `crates/web_preview/Cargo.toml` (+ `agent_ui` dep),
`crates/web_preview/MANUAL_TESTING.md`

**What it adds:**
- **v3 — React + Vue click-to-source.** `Framework` now has `React`/`Vue`; `detect_framework` does a
  single probe distinguishing Svelte (`__svelte_meta`) / Vue (`data-v-inspector`) / React
  (`__reactFiber$*`). React resolver walks the fiber for `__source`
  (`@babel/plugin-transform-react-jsx-source`; falls back across `memoizedProps`/`pendingProps`/
  `_debugSource` and the `_debugOwner`/`return` chain — React 19 removed `_debugSource`). Vue
  resolver reads `data-v-inspector="file:line:col"` (`vite-plugin-vue-inspector`).
- **v2 — CSS write-back to source.** The session subscribes to `CSS.styleSheetAdded` (before
  `CSS.enable`, so the replay isn't missed) and records `styleSheetId → {sourceURL, isInline,
  origin}`. The CSS panel now loads the most-specific **matched rule** (not just inline) and offers
  **Write to source**: deterministic edit of plain external `.css`/`.scss` (via
  `Project::open_local_buffer` → `Buffer::edit` → `save_buffer`), or an **agent fallback** for
  inline/scoped/CSS-in-JS — best-effort opens the agent panel pre-filled with a prompt
  (`agent_ui::AgentPanel` → `active_conversation_view` → `active_thread` → `message_editor.insert_text`),
  else copies the prompt to the clipboard and toasts.

**Key decisions (and why):**
- **Agent fallback uses the public `agent_ui` panel surface, not the edit-tool internals.**
  `EditFileTool` requires a live agent `Thread` + tool-call stream — wrong fit. The
  `AddSelectionToThread` pattern (focus panel + `insert_text`) is public and stable, so coupling is
  acceptable. Auto-open degrades to clipboard+toast when the panel isn't present.
- **Subscribe to `CSS.styleSheetAdded` before `CSS.enable`** — `enable` replays existing sheets, so
  subscribing after would silently miss them and break write-back source mapping.
- **Re-fetch matched styles after each live apply** — keeps the CDP edit range valid as text shifts.

**Gotchas logged:**
- `workspace::NotificationId` lives in `workspace::notifications`, not `gpui`.
- In an `AsyncApp` spawn, `Entity::update` returns the value directly (here a `Task`), so it's
  `project.update(...).await?`, not `project.update(...)?.await`.
- `focus_panel`/`insert_text` need a real `&mut Window`; threaded the `Window` from the button's
  `on_click` listener through `write_to_source` → `write_via_agent` → `open_agent_with_prompt`.

**Verification:** `web_preview` compiles clean; `clippy -D warnings` clean; `cargo fmt --check`
clean; 7/7 unit tests pass. Full-app build/runtime still gated by the missing Metal toolchain.

**Extension points:** matched-rule write-back currently targets the single most-specific rule;
multi-rule editing and `.vue`/`.svelte` `<style>`-block range mapping are the next increment.

## 2026-06-08 — `web_preview` keybindings + multi-rule CSS editing

**Files touched:** `crates/web_preview/src/web_preview.rs`, `crates/web_preview/src/css_panel.rs`,
`assets/keymaps/default-{macos,linux,windows}.json`, `crates/web_preview/MANUAL_TESTING.md`

**What it adds:**
- **Keybindings.** New `ToggleWebPickMode` action (pick mode was button-only) + a `WebPreview` key
  context on the view. Default bindings in all three keymaps: open preview
  `cmd-k cmd-shift-v` / `ctrl-k ctrl-shift-v` (Workspace); toggle pick mode
  `cmd-shift-i` / `ctrl-shift-i` (WebPreview context).
- **Multi-rule CSS editing.** The CSS panel now lists *every* matched rule (most specific first) plus
  inline, each in its own buffer-backed editor with a selector label and a per-rule **Write** button.
  Each rule applies live independently and re-fetches its own range after apply. Write-back is routed
  per rule: a precomputed `WriteRoute::Source(path)` for plain external stylesheets vs
  `WriteRoute::Agent` for everything else.

**Key decision — no fake scoped-block write-back.** Research (Chrome's CSS-in-JS DevTools docs)
confirmed Svelte/Vue scoped `<style>` under Vite are *constructed* stylesheets with **no file-backed
source range** — editable in memory only. Forcing a deterministic write into the `.svelte`/`.vue`
file would be guessing the location, so those rules are honestly routed to the agent fallback
instead. Deterministic write-back is reserved for plain external `.css`/`.scss` with a real
`sourceURL`.

**Gotchas logged:**
- `Editor::for_buffer` needs a real `&mut Window`, and rules are loaded from a windowless async pick
  handler — so `load_for_node` now takes a `Window`, obtained via the view's stored window handle
  (`window_handle.update(...)`) and `cx.spawn_in`/`update_in`.
- `language_for_name` returns a `Future`; resolved opportunistically with `FutureExt::now_or_never`
  (CSS highlighting attaches once the language is cached — fine to skip on a cold first pick).

**Verification:** compiles clean; `clippy -D warnings` clean; `cargo fmt --check` clean; 7/7 tests
pass; all three keymaps parse and carry both bindings. Full-app build/live run still gated by the
missing Metal toolchain.

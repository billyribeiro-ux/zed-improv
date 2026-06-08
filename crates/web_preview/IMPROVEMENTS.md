# Looking Glass — Hardening Backlog

_Source-verified review (adversarially checked against the code). Working through this in batches._

I have all the data I need to synthesize the plan. Let me filter, de-duplicate, and organize.

# Looking Glass (web_preview) — Prioritized Improvement Plan

Filtered to `confirmed` and `uncertain` verdicts only. Dropped 4 `false_positive` findings (on_node_picked partial-failure, Vue Windows-path split, selector-only nth-child fragility, establish_session `.ok()` leak). The mid-session-disconnect and reconnect findings appeared in 3 dimensions each — de-duplicated below.

---

## Correctness & robustness

**[HIGH] Mid-session disconnect freezes the UI on "Live" with no recovery**
Problem: `SessionState::Failed` is written only once, in the initial `connect()`. After a successful connect, nothing flips it back. When Chrome crashes, the page closes, or the dev server restarts, the read pump dies but its subscriber senders are never dropped — so `frames.next()` hangs forever (it does *not* yield `None`), the last frame stays on screen, the pill still reads green "Live", and clicks/pick-mode silently no-op into a dead socket via `.log_err()`. No reconnect path exists; `connect()` is called exactly once from `new()`.
Fix: Make the CDP read pump signal disconnection (a `closed` flag or a disconnect channel the view awaits; also clear/close subscriber channels when the pump terminates). On disconnect set `SessionState::Failed`/`Reconnecting`, `cx.notify()`, and surface it in the header. Detect via any `cdp.send()` returning the "connection closed" error (in `ack`, `forward_click`, `toggle_pick_mode`). Optionally observe the `ChromeProcess` child via `try_status`.
Effort: large (de-duplicated across correctness-cdp, ux-product disconnect, ux-product reconnect findings).

**[HIGH] Screencast frame loop leaks one GPU atlas tile per frame for the whole session**
Problem: Each decoded frame is a fresh `Arc<RenderImage>` with a monotonically-increasing id; `img()`'s `paint_image` inserts a sprite-atlas tile keyed by that id and never removes it. `RenderImage` has no `Drop` atlas eviction. The loop does `latest_frame.replace(frame)` and drops the previous frame on the Rust heap but never calls `window.drop_image`. At `everyNthFrame: 1` this leaks a tile per page frame indefinitely, growing GPU memory continuously. The livekit `RemoteVideoTrackView` is the in-repo reference that does this correctly.
Fix: Mirror livekit: keep `current_rendered_frame`/`previous_rendered_frame` fields; in `render()` call `window.drop_image(previous)` once a newer frame is painted (guard `frame.id != current.id`); register `cx.on_release` to drop retained frames on teardown. `render()` already receives `_window`, so it's available. web_preview uses `img()` on all platforms, so it needs the fix unconditionally.
Effort: medium.

**[HIGH] Click coordinates ignore `ObjectFit::Contain` letterboxing**
Problem: The frame paints as `img(image).size_full()`, which defaults to `Contain` and letterboxes to preserve aspect ratio. But `forward_click` captures bounds from a `canvas(...).size_full()` overlay filling the whole pane and passes the full pane rect + size into `image_to_page_coords`, which assumes the bitmap exactly fills the passed rect. Whenever pane aspect ratio differs from device aspect ratio (the common case), clicks are offset by the letterbox margin and scaled wrong, landing on the wrong page coordinates. The existing tests only use matching aspect ratios, so they never catch it.
Fix: Compute the contained rect via `ObjectFit::Contain.get_bounds(pane_bounds, image_size)` (the same function `img` uses) from the decoded frame's `device_width`/`device_height`, pass that rect's origin and size into `forward_click`/`image_to_page_coords`, and reject clicks in the letterbox margin.
Effort: medium.

**[MED] Chrome uses a fixed debug port + shared user_data_dir — second panel or stale process collides**
Problem: `launch` always uses the configured fixed port (default 9222) and a hardcoded `temp_dir()/zed-web-preview-chrome` profile dir. `ChromeProcess::Drop` only best-effort `kill`s (skipped on a hard Zed crash). A leaked Chrome still bound to the port causes `discover_page_ws_url` to attach to the *old* browser via `/json/list` and drive the wrong instance. Two Looking Glass panels collide identically. The doc comment admits the constraint without guarding it. (Reported in both correctness-cdp and config-packaging — same root cause.)
Fix: Unique `user_data_dir` per session (append entity id / random suffix) and an ephemeral port read back from that dir's `DevToolsActivePort` file, or verify the discovered target belongs to the just-spawned child before attaching. The dir must vary too — Chrome keys single-instance behavior off the profile dir, so varying only the port is insufficient.
Effort: medium.

**[LOW] CDP `send()` has no timeout — a wedged-but-alive Chrome hangs awaiters forever**
Problem: `send()` registers a oneshot and `rx.await`s with no deadline. It resolves only on a matching response id or the read pump draining `pending` on socket close. An alive-but-unresponsive browser (hung renderer, dropped reply) fires neither, so session setup (`*.enable` loop, `detect_framework`) blocks forever. The crate already uses `executor.timer()` for deadlines elsewhere.
Fix: Wrap `send()` in a bounded timeout (race against `executor.timer`) that removes the pending entry and returns an error on expiry. Narrow gap (requires misbehaving-not-crashed browser) but cheap and idiomatic.
Effort: small.

**[LOW] `image_to_page_coords` doesn't guard zero device dimensions or clamp negative page coords**
Problem: Guards `image_width/height==0` and `page_scale_factor==0`, but `device_width/height==0` (transient post-navigation) collapses clicks to page origin, and `page_y` can go negative when `offset_top > device_y`. Both flow straight into `Input.dispatchMouseEvent` unclamped.
Fix: Treat zero/invalid device dimensions as "no usable frame" and skip dispatch; clamp/reject out-of-viewport page coordinates before dispatching. (Note: the negative-`page_x`-from-letterbox sub-case is unreachable in the current layout — the realistic case is negative `page_y` via `offset_top`.)
Effort: small.

**[LOW] Transient frame-ack failure is swallowed; stream silently freezes** *(uncertain)*
Problem: `ack` is sent before decode and `.log_err()`-swallowed. If ack fails, Chrome stops sending frames and the stream freezes with only a warn log and no state transition. Caveat: a failing `send` usually means the socket is already dead (handled by the read pump), so the unhandled window is narrow. The finding's larger "unbounded queue growth" claim is **wrong** — the screencast is ack-gated (ack sent before decode throttles Chrome to ~1 frame in flight), so a slow decoder cannot grow the channel; only per-frame latency grows. The coalescing suggestion is therefore unnecessary.
Fix: Treat ack failure as stream-fatal — transition `SessionState` instead of swallowing. Small hardening; skip the coalescing idea.
Effort: small.

---

## Source mapping

**[HIGH] `resolve_abs_path` only handles project-relative paths — absolute, monorepo, and `src/`-skew paths silently fail**
Problem: It joins the framework-reported `file` onto each worktree's `abs_path` and checks `.exists()`. (1) Absolute paths (Vue `data-v-inspector`, React `__source.fileName`) make `PathBuf::join` discard the worktree root and return the path verbatim — works only when the local FS path equals the dev-server host path (fails for SSH/remote/devcontainer worktrees). (2) Monorepos: dev server in `apps/web/` reports `src/...` but the worktree root is the monorepo root, so the join misses. (3) Vite `/@fs/`, `/src/`, `./` prefixes don't match disk layout. Failure is silent (`log::warn` only). `css_panel`'s sibling resolver only strips the host and is divergent — two reinvented resolvers.
Fix: (a) if `file` is absolute and exists, use directly; (b) else direct worktree join; (c) else genuine suffix/tail matching over worktree files (fixes monorepo + `src/` skew), stripping Vite prefixes first. Unify with the css_panel resolver. (Suffix matching won't fix the SSH/remote sub-case — that's a different host.)
Effort: medium.

**[HIGH] Framework detected once at connect, never re-detected — hydration race + SPA navigation misclassify as Unknown**
Problem: `detect_framework` runs once after `*.enable`, before any page-load gate (only the dev server's HTTP readiness is awaited, not the browser load). It probes client-side hydration markers (`__svelte_meta`, `__reactFiber$`, `data-v-inspector`); if it runs pre-hydration, framework is permanently `Unknown` and every pick degrades to selector-only, silently. No `Page.frameNavigated`/`loadEventFired` subscription exists, so reloads and SPA route changes to differently-built routes never re-probe. The result is load-bearing — it selects the resolver.
Fix (smallest robust): re-run `detect_framework` per pick when `framework == Unknown`. Additionally subscribe to `Page.frameNavigated` in `spawn_event_loops` to cover navigation-to-different-route and stale misclassification (not just Unknown).
Effort: medium.

**[MED] Framework detection scans every DOM element with per-node key enumeration — O(N·keys) worst case**
Problem: `detect_framework` does `querySelectorAll('*')` and, for the React branch, `for (const key in el)` on every element until a `__reactFiber$` key appears. On a no-React page (not-yet-hydrated, prod build, plain HTML) it visits all N elements and enumerates hundreds of inherited keys each — a multi-hundred-ms synchronous main-thread block inside the page, run inline (blocking the first frame) with no CDP timeout. (Svelte/Vue short-circuit early on the first marker; the genuine worst case is the no-marker page.)
Fix: Use a single global probe (`[data-reactroot]` / `window.__REACT_DEVTOOLS_GLOBAL_HOOK__` / `data-v-inspector` / `__svelte_meta` on `document.body`) plus an iteration cap (`Math.min(all.length, 2000)`). Pairs naturally with the re-detect fix above.
Effort: medium.

**[MED] React resolver depends on `__source`/`_debugSource` that stock Vite + React 19 doesn't provide**
Problem: The resolver reads source only from `memoizedProps.__source`, `pendingProps.__source`, or `fiber._debugSource`. The automatic JSX dev runtime (`jsxDEV`) passes source as a separate argument (not a prop); React 18 populated `_debugSource` from it, but React 19 removed `_debugSource` and drops the argument. So on stock Vite + React 19 all three reads are empty, the resolver returns null, and picks silently fall back to selector-only despite detection correctly reporting "react". The doc comment overstates that the React path works in the default config. (The `memoizedProps.__source` path is *not* dead — classic-transform / click-to-component setups inject it.)
Fix: Mark React best-effort in the comment; emit a specific hint ("no `__source` metadata — needs classic JSX transform or a click-to-component plugin") instead of the generic info line when detection==React but resolve returns null. Optionally de-dup the ancestor walk. (The O(depth²) claim is overstated — the inner walk is capped at 50 hops, so it's O(depth).)
Effort: medium.

**[MED] `source_url_to_abs_path`'s host-strip mangles `file://` and `/@fs/` URLs**
Problem: After stripping the scheme it unconditionally `split_once('/')` to drop the "host". For `file:///Users/me/proj/src/app.css` this yields `Users/me/proj/src/app.css` (wrong relative path); `/@fs/Users/...` becomes `@fs/Users/...`. The `trim_start_matches('/')` runs *after* the host-strip so it can't recover. Mangled paths don't `exist()`, so the route falls back to the Agent prompt even when the file is in a worktree. (Lower than medium in practice — Vite usually serves CSS at `http://localhost:PORT/src/...`, the correct case, or constructs styles in-memory with no sourceURL.)
Fix: Special-case `file://` (remainder is the absolute path — use directly if it exists) and `/@fs/<abs>` (strip prefix). Apply host-strip only to `http(s)://host[:port]/...`. Add the same suffix-match fallback as `resolve_abs_path`.
Effort: medium.

**[MED] Svelte `__svelte_meta.loc` line numbers are pre-preprocessor — docs overstate "exact"**
Problem: The module doc says "exact `file:line:column` deterministically", but `loc` is emitted by the compiler *after* preprocessors run, and Svelte doesn't map it back through the preprocessor sourcemap. For `<script lang="ts">`, MDsveX, or markup-level PostCSS, `loc.line` can be offset from the on-disk file. The code feeds it straight into a `TextPoint` with no adjustment, validation, or caveat. Exact for plain unpreprocessed `.svelte`; skewed for preprocessed. (Real impact is moderate — TS-only stripping often preserves line counts; meaningful skew comes from line-count-changing transforms.)
Fix: Soften "exact" to "exact for unpreprocessed components; approximate for preprocessed ones" in both the `source_map.rs` header and the `open_source` comment. No silent fallback — detecting `lang="ts"` from `loc` alone isn't possible, so honesty is the fix.
Effort: small.

**[LOW] `parse_location` accepts `line == 0`, opening the wrong position instead of degrading**
Problem: `parse_location` does `as_u64()` with no lower bound, so a JSON `line: 0` (reachable via Vue's unguarded `parseInt` or a Svelte `loc` with line 0) passes and `saturating_sub(1)` opens the file at row 0 — a silent wrong-position rather than a graceful degrade, violating the documented 1-based invariant. (The finding's NaN framing is misleading — NaN→null is already handled by `as_u64()`; the real gap is narrowly `line == 0`. React can't produce it.)
Fix: In `parse_location`, return `None` when `line == 0` so it degrades to selector-only.
Effort: small.

---

## CSS editing

**[HIGH] Deterministic write-back uses UTF-8 byte columns for CDP UTF-16 ranges**
Problem: CDP `SourceRange` columns are UTF-16 code-unit offsets, but `write_deterministic` feeds them straight into `language::Point` (UTF-8 byte offsets) via `Point::new` + `clip_point`. For any line with non-ASCII before/within the edited block (`content: "→"`, emoji comment, accented selector), byte column ≠ UTF-16 column, so `clip_point` lands at the wrong offset and `Buffer::edit` replaces the wrong span, silently corrupting the file (`clip_point` clamps to a valid boundary, it doesn't convert encodings).
Fix: Treat CDP columns as UTF-16 — build `PointUtf16 { row, column }` and use `buffer.clip_point_utf16(Unclipped(..), bias)` (or the buffer's `offset_utf16` APIs) to get a byte `Point` before `Buffer::edit`. Add a multibyte-CSS-line test.
Effort: medium.

**[HIGH] Deterministic write applies the browser's in-memory CDP range to the current on-disk file**
Problem: `target.{start,end}` come from CDP, whose stylesheet text is whatever the *browser* loaded (post dev-server transform, possibly pre-HMR-stale). `write_deterministic` applies that range to the *current on-disk* buffer. If disk has diverged (user edited in Zed; Vite served a transformed/bundled CSS with different line layout), the range points into the wrong-but-valid region and `Buffer::edit` clobbers unrelated source. No hash/length guard that the span actually contains the reported declaration block.
Fix: Before editing, read buffer text in `[start..end]` and confirm it matches the declaration block CDP reported (compare against the pre-edit `cssText` captured at load). On mismatch, fall back to `write_via_agent` instead of editing blind. At minimum route to Source only when the sourceURL has no transform query.
Effort: large.

**[HIGH] Editing one rule invalidates sibling ranges in the same stylesheet, but only the edited rule is refreshed**
Problem: When two matched rules live in the same sheet (common: external `app.css`), editing rule A via `setStyleTexts` shifts the byte/line offsets of every later rule including B. `apply_live` re-fetches all rules but writes back only `rules[index].target`. B's `EditTarget` is left stale, so a later live edit to B sends an outdated range (fails or overwrites the wrong span) and a deterministic Write of B uses the stale range against source.
Fix: In `apply_live`'s re-fetch, update *every* entry's target in one pass (the full set is already fetched), identity-matched per the index-stability item below.
Effort: small.

**[HIGH] `setStyleTexts` + full re-fetch fire on every keystroke (no debounce)**
Problem: The `BufferEdited` subscription calls `apply_live` per edit with no debounce. Each keystroke does two round-trips (`setStyleTexts` + a full `getMatchedStylesForNode` returning all matched rules) over the websocket. Mid-token text (`colo` before `color`) fails CDP parsing, producing a warn log per keystroke. Because the task is stored in `_apply_task`, a new keystroke cancels the prior task, so an in-flight re-fetch can be aborted before it writes the fresh range — leaving `entry.target` stale exactly when it matters. (No verdict was attached to this finding, but it is consistent with the confirmed re-fetch/range mechanics below; treat as a strong candidate, verify the cancellation race.)
Fix: Debounce `apply_live` behind a ~150ms timer reset per keystroke; re-fetch once after settle; key results by `styleSheetId+selector` not positional index; swallow expected mid-token parse errors.
Effort: medium.

**[MED→LOW] `apply_live` re-aligns re-fetched rules by positional index** *(uncertain)*
Problem: After `setStyleTexts`, the code joins `loaded.get(index)` → `rules[index]` with no identity check. The positional join is genuinely unguarded. **But** the finding's two stated corruption triggers don't hold: `setStyleTexts` changes only declaration *values*, never selectors, so it cannot reorder `matchedCSSRules` (membership depends on selector matching, not cascade winners) — the "cascade reorders rules" mechanism is factually wrong. The only residual risk is the inline-style-at-index-0 case if CDP drops `inlineStyle` after an edit empties it, which couldn't be confirmed and mis-targets at most one entry, not all.
Fix: Match re-fetched rules to existing entries by stable identity (`styleSheetId` + declaration-block start range, or `styleSheetId` + selector text); leave the entry untouched on no-match. Defensive hardening — re-classify from high to low severity, not a confirmed data-corruption bug.
Effort: medium.

**[MED] Live-apply CDP errors are only logged — no user feedback** *(no verdict attached; consistent with confirmed file-wide `.log_err()` pattern)*
Problem: `setStyleTexts` failure (invalid CSS, stale range, detached node) is swallowed into `log::warn!` with no UI surfacing, violating the project rule that errors propagate to the UI. The user can't distinguish an expected typo from a terminal failure (read-only stylesheet, node gone). Contrast `write_deterministic`, which sets `Status::Error`.
Fix: Distinguish "CSS not yet valid" (don't surface) from terminal failures (set a transient status / subtle inline indicator on the rule row). Pairs with the debounce above so only settled-text failures are considered.
Effort: medium.

**[MED] `route_for` chooses the first existing path-tail match — ambiguous and transform-blind** *(no verdict; overlaps confirmed `source_url_to_abs_path` issues)*
Problem: Mapping is purely textual — strips scheme, drops the first segment as "host", joins onto each worktree, returns the first that exists. (1) For `file://`/no-host URLs the first real directory segment is wrongly discarded. (2) Vite serves CSS at `/src/app.css?t=...` mapping to *transformed* output with different line layout, yet a source file coincidentally exists and is chosen as deterministic — feeding the wrong-range write-back. The `?query` transform marker that signals "processed, don't write deterministically" is stripped away.
Fix: Treat as Source only for plain `http(s)` paths with no transform/query marker; prefer longest-suffix match; bail to Agent on ambiguity (>1 worktree match). Keep the query string to detect dev-server transforms. (Overlaps the `source_url_to_abs_path` fix — implement together.)
Effort: medium.

**[MED] Every pick rebuilds all buffers, editors, and LSP subscriptions from scratch** *(no verdict)*
Problem: `load_for_node` clears `self.rules` and news a fresh `Buffer` + `Editor::for_buffer` + `cx.subscribe` per matched rule on every pick. Rapid picking churns CSS-language-server attach/detach and GPUI entities continuously; `Editor::for_buffer` is not cheap.
Fix: Pool/reuse editors keyed by `styleSheetId+selector` when a pick shares rules; or build into a temp Vec and swap to avoid a visible empty frame. Measure attach/detach cost before optimizing.
Effort: large.

**[MED] `suppress_apply` is dead state; its comment describes a path that doesn't exist** *(no verdict)*
Problem: `suppress_apply` is documented as guarding programmatic buffer resets and is read in the `BufferEdited` subscription, but it's never set to `true` anywhere. Harmless dead state today, but the moment someone adds `buffer.set_text` after re-fetch (the natural canonicalize step) it recursively re-triggers `apply_live` with no guard engaged — an infinite apply loop.
Fix: Either delete the field/guard (and fix the comment), or wire it: set `true` around any programmatic editor/buffer mutation, reset after.
Effort: small.

**[LOW] CSS language loaded via `now_or_never`, silently dropped if not ready** *(no verdict)*
Problem: `language_for_name("CSS").now_or_never().and_then(Result::ok)` returns `None` if the registry future hasn't resolved (cold start). The first pick then gets plain-text editors with no highlighting/LSP, and since editors rebuild per pick it "fixes itself" later — intermittent and confusing.
Fix: Await the language future in the spawned load task and pass the resolved `Option<language>` into `build_rule_editors`.
Effort: small.

**[LOW] Inline-style entry has no selector identity for re-matching** *(no verdict)*
Problem: The inline style sits at index 0 with `selector: None` and appears/disappears based on `matched.inlineStyle` presence, making it the most fragile index. Any identity-based re-match (per the index-stability fix) can't key on selector for it.
Fix: Give the inline entry a stable synthetic identity (`styleSheetId` + an "inline" sentinel) so re-fetch matching finds it even when its text is empty, and never positionally shift matched rules when it toggles.
Effort: small.

---

## UX & product

**[HIGH] No reconnect / retry affordance after a failed or lost connection**
Problem: `connect()` is called once from `new()`. On failure the panel dead-ends at "Couldn't connect" + raw error; the Failed onboarding branch and the header pill have no Retry button and no auto-retry. The only recourse is closing the tab and re-running `OpenWebPreview`, which loses panel state. The most common real path (open the panel before `npm run dev` is ready, or after a dev-server restart) is unrecoverable in place.
Fix: Add a Retry/Reconnect button to the Failed onboarding state and header pill that runs a reset-then-`connect()` path: clear `latest_frame`, drop the old `_chrome`, set `Connecting`, re-run `connect()`. Avoid leaking old event-loop tasks (`connect()` pushes onto `_tasks` rather than replacing). The "lost connection" case also needs the disconnect detection from the Correctness section to drive the view back to Connecting/Failed.
Effort: medium (de-duplicated with the disconnect item — these are two halves of the same recovery story).

**[HIGH] Error states don't tell the user how to fix the specific failure**
Problem: The Failed state renders the raw `format!("{error:#}")` anyhow chain verbatim under one generic "Couldn't connect" headline. The three root causes need different fixes (dev server not started → `npm run dev`; Chrome not found → install/set `chrome_path`; port 9222 in use → change `remote_debugging_port`) but collapse into one wall of text. The actionable `chrome.rs` message exists but is buried mid-chain; the env-check script and MANUAL_TESTING.md diagnose all three but none reaches the in-app state.
Fix: Classify the failure at the `establish_session`/`connect` call sites into a small enum and render a tailored headline + remediation step, showing the configured URL/port explicitly. (Skip the "deep link" part of the suggestion — no settings-deep-link primitive exists; the enum + tailored text is the actionable core.)
Effort: medium.

**[MED→HIGH] No way to change the dev-server URL from the UI — settings-only**
Problem: The URL is read once from `WebPreviewSettings` and editable only by hand-editing `settings.json`. No URL bar, no inline edit, and no display of the target URL in the Connecting/Connected states. A user on a non-default port must know the setting key exists, edit JSON, and reopen the panel. (Correction to the original finding: the Failed-state error *does* name the URL via the anyhow context chain and renders it — so "doesn't even name the URL" is false. The real gap is the inability to *change* the URL/reconnect in-panel, plus no URL display in non-failure states. Downgrade to medium.)
Fix: Add an editable URL field to the header that updates the effective URL and reconnects on Enter; show the target URL in the Connecting/Connected states. Note: no reconnect path exists yet, so this is net-new, pairing with the Retry item above.
Effort: large.

**[MED] No reload / refresh / navigation controls**
Problem: The header has only pick-mode and help buttons. No reload, back/forward, or address affordance. When HMR fails or a hard refresh is needed, there's no in-panel way to reload — the panel feels like a read-only screenshot. The fix is trivial over CDP: `SessionState::Connected` already holds the live `CdpClient`, the `Page` domain is already enabled, and `send` is generic.
Fix: Add a reload `IconButton` to the header sending `Page.reload {ignoreCache:false}`, gated on `SessionState::Connected` (mirror `forward_click`'s early-return). Back/forward via `Page.navigateToHistoryEntry` as follow-up.
Effort: small.

**[MED] Onboarding/help shortcut mismatch + pick-mode binding shadowed when the CSS editor is focused**
Problem: `ToggleWebPickMode` (⌘⇧I) is bound under the `WebPreview` context, but the default keymap also binds ⌘⇧I to `editor::Format` under the deeper `Editor` context. After the user clicks into a CSS editor (the documented pick→edit→pick flow), GPUI resolves ⌘⇧I to the deepest-context match, so it *reformats the CSS* instead of toggling pick mode. `on_node_picked` never restores focus to the preview, so there's no mitigation. The onboarding/help text promises a shortcut that silently breaks mid-workflow. (Mechanism correction: ancestor contexts stay in the dispatch stack — the cause is binding *shadowing* by the deeper Editor context, not the WebPreview context leaving the stack.) The help "⌘K ⌘⇧V Open Looking Glass" row is a minor redundancy nit, not a bug.
Fix: Rebind `ToggleWebPickMode` to a chord that doesn't collide with `editor::Format`, or broaden its context so it survives editor focus inside the panel, and/or restore focus to the preview after a pick.
Effort: medium.

**[MED] No loading spinner during the multi-second connect**
Problem: Connecting (dev-server poll up to 30s, Chrome launch, CDP handshake, framework detection, first frame) shows only a static text label and a static muted Screen icon — nothing animates, so the panel looks hung. The CSS panel's "Loading styles…" is the same. Spinner primitives exist (`IconName::LoadCircle` + `with_rotate_animation`, used in `file_finder`).
Fix: Show an animated spinner in the Connecting, "Waiting for the first frame…", and CSS Loading states. (The per-phase progress-text sub-suggestion — "Starting browser…" → "Attaching…" — is larger than small; `establish_session` currently writes only terminal Connected/Failed state, so it needs new phase plumbing. Treat phase text as an optional follow-up.)
Effort: small.

**[MED] Pick-mode state is under-communicated — no in-canvas cue, no Esc to exit**
Problem: The only in-Zed feedback for pick mode is the header magnifier's toggle tint. No cursor change, border, or banner in the canvas. In pick mode `forward_click` early-returns, so normal clicks stop working — a user who forgot pick mode is on thinks the preview froze. No Esc/cancel; `picking` clears only on a successful selection or another toggle. (Aggravating detail: no `mouseMoved` is forwarded to Chrome, so Chrome's own searchForNode highlight doesn't track the cursor over Zed's static image — there's effectively zero in-canvas affordance.)
Fix: Gate an in-canvas border/overlay + banner ("Pick mode — click any element to open its source · Esc to cancel") on `self.picking` in the render preview branch; bind Esc to a cancel action in the `WebPreview` context.
Effort: small.

**[MED] CSS rule list: no element identity, no list keyboard nav, silent deterministic Write**
Problem: (1) The panel header is a static "Styles" label with no picked-element summary (the struct stores only `node_id`, never tag/id/class). (2) The rule list is a plain scroll container of editors — no list role, roving focus, or arrow-key nav. (3) Deterministic Write to source succeeds *silently* (no toast), while the agent clipboard-fallback branch *does* toast — the happy path gives less feedback than the fallback. (4) On write error the whole panel body is replaced with red text, removing the in-progress editors from view (they persist in `self.rules` but are un-rendered). (Correction: the agent-panel-opened branch is also silent, so "agent always toasts" overstates it; and editors are un-rendered, not destroyed.)
Fix: Show a picked-element summary header (capture tag/id/class at pick time); add a success toast/inline checkmark on deterministic Write; surface write errors inline on the affected rule instead of wiping the panel; make the rule list keyboard-navigable.
Effort: medium.

**[MED] No ARIA/accessibility primitives — canvas unlabeled, no keyboard path, status changes unannounced**
Problem: The preview img+canvas has only `on_mouse_down` — no role, accessible name, or keyboard activation, so a keyboard/screen-reader user can't interact at all (everything is mouse-coordinate based). Onboarding steps and the CSS rule list have no list semantics; status and pick-mode transitions aren't announced. GPUI supports these (`.role()`, `.aria_label()`, `on_a11y_action` via AccessKit) — they're simply unused. (Context: this is a codebase-wide gap — app-level panels broadly haven't adopted the a11y API — not a Looking-Glass-specific regression. Use GPUI/AccessKit terms, not literal HTML ARIA.)
Fix: Give the canvas a role + accessible name; list semantics for steps and rules; announce status/pick-mode changes; define a keyboard path between the two panes. Audit against WCAG 2.2 AA. (Large — keyboard interaction for an embedded screencast is genuinely nontrivial.)
Effort: large.

**[LOW] Help overlay is incomplete and not Esc-dismissible**
Problem: Lists only two shortcuts + one paragraph. Omits the interaction model (plain click forwards to the page), how to exit pick mode, the deterministic-Write-vs-"scoped · agent"-fallback distinction (a confusing label users hit), and status meanings. The overlay is a non-modal floating div over the preview with no backdrop, no dialog semantics, no focus trap, and dismissible only by re-clicking the Info button — no Esc.
Fix: Expand to cover the full model; make it Esc-dismissible with a close button and dialog semantics; ensure it doesn't obscure without an obvious exit.
Effort: small.

**[LOW] Picking an element with no deterministic source gives no user feedback**
Problem: `Resolution::SelectorOnly` (unknown framework, prod build, non-component node) produces only a `log::info!`. From the user's view: entered pick mode, clicked, pick mode turned off, nothing happened — reads as broken even though it's the graceful-degradation path. (The CSS panel *does* populate independently, but nothing ties it to the pick.)
Fix: Show a transient toast on `SelectorOnly` ("No source mapping for this element — its styles are loaded on the right"). The exact toast idiom already lives at `css_panel.rs:454-464`.
Effort: small.

**[LOW] Status-label wording is inconsistent across header, onboarding, and MANUAL_TESTING.md**
Problem: The header shows "Live · Svelte"/"Disconnected"; MANUAL_TESTING.md tells the tester to expect "Connected (Svelte)"/"Connected · React". None of the doc strings are ever produced. Within the source, the same `Failed` state shows "Disconnected" in the pill but "Couldn't connect" in onboarding. "Disconnected" is semantically wrong for an initial connection that never succeeded. (Correction: `Framework::Unknown.label()` *is* defined as "unknown framework" — the finding's "unspecified" claim is wrong; the drift is only the "Live"/"Connected" prefix.)
Fix: Pick one vocabulary and align header, onboarding, and the checklist. Use "Couldn't connect" for initial failure; reserve "Disconnected" for a lost mid-session connection once that state exists.
Effort: small.

---

## Config & packaging

**[HIGH] Apply script never inserts keymaps — clean runs always exit 1**
Problem: `add_keymap()` only checks whether the binding exists; if absent it calls `warn()` (sets `NEEDS_MANUAL=1`) and echoes instructions to stdout — it never writes the keymap JSON. On a fresh upstream checkout (bindings absent) the script *always* sets `NEEDS_MANUAL` and exits 1, contradicting UPDATING.md's "re-applies cleanly" and forcing the user to hand-edit three JSON files every update. This is the script's biggest reliability gap.
Fix: Actually insert via a jq/python step that appends `{"context":"WebPreview","bindings":{...}}` (and a global block for `OpenWebPreview`) when the action is absent, or a real anchored heredoc insert. Also fix the dangling `keymap-bindings.md` reference (below) in the same change.
Effort: medium.

**[HIGH] Documented dev-channel icon swap is not implemented in the apply script**
Problem: UPDATING.md lists `crates/zed/resources/app-icon-dev.png + @2x.png` as part of the feature footprint and documents `bundle-mac -i` as the install step, and the modified icons + `.orig` backups exist on disk (confirmed different from upstream). But the apply script has *zero* icon logic, and the tarball ships only the crate-local `crates/web_preview/resources/app-icon.png` — *not* the `crates/zed/resources/app-icon-dev*.png` that `bundle-dev` reads. So a fresh checkout + apply + `bundle-mac -i` yields the stock Zed Dev icon, silently diverging from the doc.
Fix: Add an idempotent icon step: if `app-icon-dev.png.orig` is absent, back up upstream icons to `*.orig`, then copy the Looking Glass `png/@2x` (carried in the tarball/script) into `crates/zed/resources/`. Guard with a marker. Or, if the swap is intentionally manual, remove it from UPDATING.md's footprint.
Effort: medium.

**[HIGH] No setting to auto-launch the dev-server command from Zed** *(no verdict attached; the underlying facts — `dev_server.rs` only polls, no `dev_command` setting, onboarding pushes `npm run dev` onto the user — are corroborated by the confirmed settings/onboarding findings)*
Problem: `dev_server.rs` only polls a URL; its own doc admits launching the dev command is "a follow-up." There's no `web_preview.dev_command`/`auto_launch` setting and no task/terminal wiring; onboarding step 1 literally tells the user to run `npm run dev` by hand. The crate already depends on project/workspace, so the task-spawn plumbing exists but is unused.
Fix: Add `dev_command: Option<String>` (and optionally `auto_launch: bool`) to `WebPreviewSettingsContent`. When set and the URL is unreachable, spawn it through the existing task/terminal infrastructure before polling, surfacing failures to the panel. At minimum, expose the setting and a "Run dev server" button in onboarding.
Effort: large.

**[MED] Fixed `user_data_dir` + fixed port make a second preview collide**
Problem: Same root cause as the Chrome-collision item in Correctness, reported here from the config angle: both the profile dir and port are single global values; `OpenWebPreview` has no singleton/dedup, so a second tab/window launches a second Chrome against the same dir+port. Chrome forwards to the existing instance and `discover_page_ws_url` attaches to the wrong one.
Fix: Per-view `user_data_dir` (entity id / random suffix) + ephemeral port (port=0 + `DevToolsActivePort`, or free-port allocation). The dir must vary too. *(Same fix as the Correctness item — implement once.)*
Effort: medium.

**[MED] Anchor-based inserts are single-point-of-failure with no verification**
Problem: Every Zed-file edit keys off one exact substring anchor (`web_search = { path = ... }`, `component_preview::init(...)`, `pub audio: Option<AudioSettingsContent>,`) with hardcoded indentation (4 spaces for the workspace member, 8 for the init line). If upstream reformats but the anchor still matches, the insert is misaligned/syntactically broken yet the script still prints "re-applied cleanly" — no parse check runs in-script. (Nuance: a *removed* anchor does fail loudly via `warn`+exit 1; the silent window is the still-matches-but-reformatted case.)
Fix: Anchor on stabler tokens (the `[workspace.dependencies]` header, the `audio:` field name) and add a verification pass (`cargo metadata` / `cargo check -p settings_content`) that fails loudly before reporting success.
Effort: medium.

**[MED] No tests or shellcheck gate for the apply script**
Problem: The script is the entire reproducibility story yet has zero automated coverage. Its idempotency claim is never asserted (nothing runs it twice against a fixture). `script/shellcheck-scripts` exists but globs only `script/` (maxdepth 1), excluding `patches/`, and isn't wired into any CI workflow at all. A regression like the keymap no-op ships undetected.
Fix: Add a fixture test that runs the script twice and asserts each marker is present exactly once, then all-skip + exit 0 on re-run. Extend `shellcheck-scripts`' glob to include `patches/` and wire it into CI. (Framing: extend/wire the existing helper, not "add shellcheck from scratch"; current scripts pass shellcheck today — the value is regression protection.)
Effort: medium.

**[MED] Screencast quality and frame rate are hardcoded with no settings**
Problem: `Page.startScreencast` bakes in `quality: 80`, `everyNthFrame: 1`, and no `maxWidth/maxHeight`, so it always streams full-rate full-resolution JPEGs. No setting lets users trade fidelity for performance on weak machines or high-DPI displays. (Correction: decode + red/blue swap runs on a *background* thread, not the main path — the "decoded on the main path" framing overstates impact. This is a missing-knob enhancement, lean toward low priority.)
Fix: Add `screencast_quality: Option<u8>` (clamp 1–100, default 80), `screencast_every_nth_frame: Option<u32>` (default 1), optionally `max_width/max_height` to `WebPreviewSettingsContent`, threaded through `screencast::start`.
Effort: medium.

**[MED] No setting to disable or override framework detection** *(no verdict attached; the underlying facts — unconditional probe, silent `Unknown` degrade, no override — are confirmed by the source-mapping detection findings)*
Problem: Detection runs unconditionally and there's no way to skip it, force a framework when auto-detect guesses wrong, or handle multi-framework pages. `Unknown` silently degrades every pick to selector-only with no user-visible reason. An override would also let React/Vue users opt in before those resolvers are battle-tested.
Fix: Add `framework: Option<String>` ("svelte"|"react"|"vue"|"auto", default auto). On a concrete value, skip the probe and use that resolver; on "auto", keep current behavior. Log a line when it falls back to Unknown.
Effort: small.

**[MED] Settings omit the operational knobs users actually need**
Problem: The surface is only url/chrome_path/remote_debugging_port. Missing: the dev-server wait timeout (hardcoded `Duration::from_secs(30)` at the call site — though `wait_until_ready` already takes a `timeout` param, so only the literal blocks it), extra Chrome launch flags (`--window-size`, proxy, `--disable-gpu` for headless CI), viewport/device-emulation, reuse-vs-launch. The 30s timeout is a common pain point (cold Vite/webpack builds exceed it) with no escape hatch.
Fix: Add `dev_server_timeout_secs: Option<u64>` (default 30, threaded into the existing param — low effort) and `chrome_args: Option<Vec<String>>` (needs a `chrome::launch` signature change — medium).
Effort: medium.

**[MED] Telemetry is a single static open event; no field-debugging signal for failures**
Problem: The only telemetry is the static string "looking glass: open". Every failure stage (dev-server-timeout, chrome-not-found, CDP-connect-failure, framework-detected, pick source-vs-selector, CSS write success) funnels into one `log::error!` line. When a user reports "it just says Disconnected," there's no aggregate signal showing whether it's Chrome, the dev server, or CDP.
Fix: Emit `telemetry::event!` at each `establish_session` phase boundary (`dev_server_ready`/`timeout`, `chrome_launched`/`not_found`, `cdp_connected`/`failed`, `framework_detected=<enum>`) and at pick resolution (source vs selector_only), with bounded enum labels. (Note: `telemetry` is not yet a dep of `web_preview` — add it.)
Effort: medium.

**[MED] Dead reference to `patches/keymap-bindings.md`**
Problem: `add_keymap()`'s warn message points users to `patches/keymap-bindings.md`, which doesn't exist (patches/ has only UPDATING.md, the script, and the tarball). (Softening: the script *does* echo the key/action pairs inline at the same moment, and UPDATING.md mentions the edit — so the user isn't fully stranded; lean low-to-medium.)
Fix: Drop the "(see patches/keymap-bindings.md)" clause since bindings are already echoed inline, or create the doc. Fold into the keymap-insertion fix above.
Effort: small.

**[LOW] Default URL/port hardcoded in three places risk drifting** *(no verdict)*
Problem: The default URL (`http://localhost:5173`) and port (9222) appear as literals in `web_preview_settings.rs`, in the injected `WebPreviewSettingsContent` doc comments, and again in `script/check-web-preview-env`. Three copies kept in sync by hand.
Fix: Define `pub const DEFAULT_URL` / `DEFAULT_REMOTE_DEBUGGING_PORT` in `web_preview_settings.rs`, use them in `from_settings`, reference in doc text; add a comment in the shell script pointing at the Rust constant.
Effort: small.

**[LOW] Cargo.toml lacks description/keywords; manifest doesn't state crate identity** *(no verdict)*
Problem: No `description`, `keywords`, or `repository`. `publish.workspace=true` means it could be published with no description, and there's no one-line statement of what the crate is in its manifest.
Fix: Add a `description`. If publishing isn't intended for this fork crate, set `publish = false` explicitly rather than inheriting the workspace default.
Effort: small.

---

## Top 5 highest-leverage improvements

1. **Detect mid-session disconnect and add Retry/Reconnect** *(medium effort, two HIGH bugs)* — The single biggest product gap. One change (CDP read-pump signals disconnect → flip to Failed/Reconnecting → header pill + Retry button → reset-then-`connect()`) fixes the frozen-"Live" zombie state *and* the unrecoverable initial-failure dead-end, the two most common real-world failure paths. Note the read pump must close subscriber channels so the frame loop stops hanging.

2. **Fix click-coordinate letterboxing** *(medium effort, HIGH)* — Clicks landing on the wrong page coordinates breaks the headline interaction (click → forward to page, pick → element). The fix is small in code (reuse `ObjectFit::Contain.get_bounds`) but load-bearing for whether the tool works at all on any non-matching aspect ratio — i.e. the common case.

3. **Fix the apply-script keymap insertion + icon swap** *(medium effort each, two HIGH packaging bugs)* — The script is the entire reproducibility story for the fork, and it exits 1 on every clean run while silently dropping the documented icon. Without these, the "patch-on-top-portable" premise is broken on the first re-apply. High leverage because it's the foundation every future update rides on.

4. **Add a reload button + a loading spinner + pick-mode in-canvas cue + SelectorOnly toast** *(all small, MED/LOW)* — A cluster of tiny, independent UX fixes that together flip the panel's perceived completeness from "read-only screenshot that sometimes looks frozen" to "live browser tool." Best ratio of perceived-product-improvement to engineering hours in the whole plan.

5. **Make framework detection robust: re-detect on `Unknown`/navigation + bound the scan + React `__source` hint** *(medium effort)* — Source mapping is the differentiating feature. Today a hydration race or SPA route change permanently downgrades every pick to selector-only silently, stock Vite+React-19 never maps, and the probe can block the first frame on large pages. Per-pick re-detect + a single global probe + a specific "no `__source` metadata" hint rescues the core value prop across the most common real setups.

Excluded from the top 5 but worth flagging as the highest-severity *correctness* items to schedule next: the two HIGH CSS write-back bugs (UTF-16 column mismatch and stale-range-vs-disk) — both can silently corrupt the user's source files, which is worse than any UX gap, but they sit behind the deterministic-write path that fewer users reach than the connect/click paths above.
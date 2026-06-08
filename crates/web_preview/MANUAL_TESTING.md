# `web_preview` — manual runtime test checklist

The `web_preview` crate is fully implemented and unit-tested, but it cannot be exercised
end-to-end in an environment without the full Xcode toolchain: building the macOS GUI app
requires the Metal shader compiler (`xcrun metal`), which is absent when only the Command Line
Tools are installed (`xcrun: error: unable to find utility "metal"`). This file is the checklist
to run once you are on a machine that can build and launch Zed.

> **Quick check:** run `script/check-web-preview-env [dev-server-url]` first — it verifies the Metal
> toolchain, Chrome, and (optionally) that your dev server is reachable, and prints exactly what to
> fix before you build.

## Prerequisites

1. **Full Xcode** (not just Command Line Tools), so `xcrun -f metal` resolves. Verify:
   ```sh
   xcrun -f metal        # should print a path, not an error
   ```
2. **Google Chrome or Chromium** installed (the preview drives an external Chrome over CDP).
   Override the binary with `web_preview.chrome_path` in settings if it is not in a default location.
3. A **SvelteKit (Svelte 5) app in dev mode**. A throwaway one works:
   ```sh
   npm create svelte@latest demo && cd demo && npm install && npm run dev
   # note the URL it prints, usually http://localhost:5173
   ```
   Svelte's compiler emits the `__svelte_meta` source metadata on DOM elements in dev mode by
   default — no extra plugin is required for click-to-source.

## Settings

If the dev server is not on the default `http://localhost:5173`, set it in Zed settings:
```json
{
  "web_preview": {
    "url": "http://localhost:5173",
    "remote_debugging_port": 9222,
    "chrome_path": null
  }
}
```

## Build & launch

```sh
cargo run            # debug build of the Zed app
# or: script/zed-local
```

## Test steps

1. **Open the panel.** Command palette → **web preview: Open Web Preview** (`OpenWebPreview`), or
   press `cmd-k cmd-shift-v` (macOS) / `ctrl-k ctrl-shift-v` (Linux/Windows). Toggle pick mode with
   `cmd-shift-i` / `ctrl-shift-i` while the preview panel is focused.
   - [ ] A "Web Preview" tab opens in the active pane.
   - [ ] Status shows `Connecting…`, then `Connected (Svelte)` once Chrome attaches and the
         framework probe succeeds. (`Connected` without `(Svelte)` means the framework probe did
         not find `__svelte_meta` — confirm the dev server is Svelte in dev mode.)
   - [ ] The running page renders **inside** the panel (streamed via CDP screencast), not only in a
         separate Chrome window.

2. **Live page interaction (input forwarding).** With pick mode OFF, click a button/link in the
   embedded preview.
   - [ ] The click takes effect in the page (navigation, counter increment, etc.), proving
         `Input.dispatchMouseEvent` coordinate mapping is correct. If clicks land in the wrong
         place, the coordinate math in `screencast::image_to_page_coords` needs the real
         device-pixel-ratio path revisited (see Known risks).

3. **Deterministic click-to-source (the headline feature).** Click the magnifier (pick mode) in the
   toolbar, then click an element in the preview.
   - [ ] The element highlights (Chrome inspect overlay).
   - [ ] Zed **opens the exact `.svelte` source file** that renders it…
   - [ ] …with the cursor on the **correct line** (1-based source → 0-based `Point` conversion).
   - [ ] Pick mode turns itself off after one selection.
   - Try a component nested several levels deep to confirm the `parentElement` walk in
     `source_map::resolve_svelte` finds the nearest `__svelte_meta`.

4. **Live CSS editing (multi-rule).** After picking an element, the right-hand Styles panel lists
   **every matched rule** (most specific first) plus the inline `style=""`, each with its selector,
   a source label, and its own editor.
   - [ ] Editing a property in any rule's editor (e.g. `color: red`) applies **live in the preview**
         via `CSS.setStyleTexts` without a reload.
   - [ ] Each rule's source label is correct: a filename (e.g. `app.css`) for plain external
         stylesheets, or `scoped · agent` for inline / scoped `<style>` / CSS-in-JS rules.
   - [ ] No flicker/crash on rapid edits (the re-fetch-after-apply keeps each rule's range valid).

4b. **CSS write-back to source (per rule).** For a rule whose label is a filename, edit it and click
   its **Write** button.
   - [ ] The edit is written into that `.css`/`.scss` file on disk (open it to confirm) and saved.
   - For a rule labeled `scoped · agent` (scoped `<style>` / CSS-in-JS / Tailwind — these are
     constructed stylesheets with no file-backed range, so deterministic write-back is *not*
     attempted), click **Write**:
     - [ ] The agent panel opens pre-filled with a prompt describing the change (if available)…
     - [ ] …otherwise a toast appears and the prompt is on the clipboard (paste to confirm).

5. **Other frameworks (v3).** Repeat steps 1–3 against a **React** (Vite, dev mode — jsx-source is
   on by default) and a **Vue** app (with `vite-plugin-vue-inspector` enabled).
   - [ ] Status shows `Connected · React` / `Connected · Vue`.
   - [ ] Pick mode opens the correct `.jsx`/`.tsx` (React) or `.vue` (Vue) file at the right line.
   - React note: if click-to-source returns nothing, confirm the babel jsx-source plugin is active
     (default in Vite React dev) — React 19 removed `_debugSource`, so the `__source` prop path is
     the one that must be present.

6. **Graceful degradation.** Point the preview at an unsupported app (or a prod build with metadata
   stripped).
   - [ ] Status shows `Connected · unknown framework`, pick mode still highlights, and the log notes
         a selector instead of opening a file — the feature degrades, it does not crash.

7. **Teardown.** Close the Web Preview tab.
   - [ ] The external Chrome process exits (killed by `ChromeProcess::Drop`); no orphaned browser.

## Known risks to watch (from the plan's de-risk notes)

- **TypeScript line skew:** with `<script lang="ts">`, `__svelte_meta.loc` line/column can be offset
  by the TS→JS transform (svelte#8360). If click-to-source lands a few lines off in `.ts`
  components, this is the cause — verify against a plain-JS component first.
- **Screencast coordinate accuracy** on HiDPI displays / when the page is scrolled — step 2 is the
  canary.
- **Chrome debugging port already in use:** if `9222` is taken, set a different
  `remote_debugging_port`.

#!/usr/bin/env bash
#
# Re-apply the Looking Glass feature on top of an upstream Zed checkout.
#
# This is the "patch-on-top" update model: after pulling Zed's updates, run this
# script to re-insert our feature. Every edit is idempotent (it checks whether the
# change is already present before applying), so it is safe to run repeatedly and
# survives most upstream churn. If an anchor has moved/changed upstream, the script
# reports exactly which edit needs manual attention instead of silently corrupting a file.
#
# Usage (from the repo root):
#   bash patches/apply-looking-glass.sh
#
# What it does:
#   1. Unpacks the self-contained `web_preview` crate (pure new files; never conflicts).
#   2. Re-applies our small edits to Zed-owned files (the only possible conflict points).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ok()   { printf '  \033[32m✓\033[0m %s\n' "$1"; }
skip() { printf '  \033[33m•\033[0m %s (already applied)\n' "$1"; }
warn() { printf '  \033[31m!\033[0m %s\n' "$1"; NEEDS_MANUAL=1; }
NEEDS_MANUAL=0

echo "Applying Looking Glass onto $(basename "$ROOT")…"

# ---------------------------------------------------------------------------
# 1. The web_preview crate — pure new files, unpack as-is.
# ---------------------------------------------------------------------------
if [[ -d crates/web_preview ]]; then
  skip "web_preview crate"
elif [[ -f patches/looking-glass-crate.tar.gz ]]; then
  tar -xzf patches/looking-glass-crate.tar.gz
  ok "web_preview crate unpacked"
else
  warn "patches/looking-glass-crate.tar.gz missing — cannot restore the crate"
fi

# Helper: insert LINE after the first line matching ANCHOR, unless MARKER already present.
insert_after() {
  local file="$1" anchor="$2" line="$3" marker="$4" label="$5"
  if ! [[ -f "$file" ]]; then warn "$label: $file not found"; return; fi
  if grep -qF "$marker" "$file"; then skip "$label"; return; fi
  if ! grep -qF "$anchor" "$file"; then warn "$label: anchor not found in $file (upstream changed — apply manually)"; return; fi
  # Insert after the first anchor match.
  awk -v anchor="$anchor" -v ins="$line" '
    { print }
    !done && index($0, anchor) { print ins; done=1 }
  ' "$file" > "$file.tmp" && mv "$file.tmp" "$file"
  ok "$label"
}

# ---------------------------------------------------------------------------
# 2. Workspace registration — root Cargo.toml.
# ---------------------------------------------------------------------------
insert_after "Cargo.toml" '"crates/web_search",' '    "crates/web_preview",' \
  '"crates/web_preview"' "Cargo.toml: workspace member"
insert_after "Cargo.toml" 'web_search = { path = "crates/web_search" }' \
  'web_preview = { path = "crates/web_preview" }' \
  'web_preview = { path' "Cargo.toml: workspace dependency"

# ---------------------------------------------------------------------------
# 3. zed crate dependency + init call.
# ---------------------------------------------------------------------------
insert_after "crates/zed/Cargo.toml" 'web_search.workspace = true' \
  'web_preview.workspace = true' \
  'web_preview.workspace = true' "zed/Cargo.toml: dependency"
insert_after "crates/zed/src/main.rs" 'component_preview::init(app_state.clone(), cx);' \
  '        web_preview::init(app_state.clone(), cx);' \
  'web_preview::init' "zed/main.rs: init call"

# ---------------------------------------------------------------------------
# 4. Settings (content struct field, struct definition, vscode importer).
# ---------------------------------------------------------------------------
insert_after "crates/settings_content/src/settings_content.rs" \
  'pub audio: Option<AudioSettingsContent>,' \
  '
    /// Configuration of the in-editor web app preview.
    pub web_preview: Option<WebPreviewSettingsContent>,' \
  'pub web_preview: Option<WebPreviewSettingsContent>' "settings_content: field"

# The WebPreviewSettingsContent struct definition (appended near the audio content struct).
if grep -qF 'pub struct WebPreviewSettingsContent' crates/settings_content/src/settings_content.rs; then
  skip "settings_content: struct"
else
  cat >> crates/settings_content/src/settings_content.rs <<'RUST'

/// Configuration of the in-editor web app preview.
#[with_fallible_options]
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema, MergeFrom, Debug)]
pub struct WebPreviewSettingsContent {
    /// URL of the running dev server to preview.
    ///
    /// Default: "http://localhost:5173"
    pub url: Option<String>,
    /// Explicit path to the Chrome/Chromium executable. If unset, common locations are searched.
    pub chrome_path: Option<String>,
    /// Port Chrome exposes its remote debugging endpoint on. The default value means "pick a free
    /// port automatically"; set a specific port only if you need a fixed one.
    ///
    /// Default: 9222
    pub remote_debugging_port: Option<u16>,
    /// Command to launch the dev server if the URL isn't already reachable (e.g. "npm run dev").
    pub dev_command: Option<String>,
    /// Override framework detection: "svelte", "react", "vue", or "auto".
    ///
    /// Default: "auto"
    pub framework: Option<String>,
    /// How long to wait (seconds) for the dev server URL to become reachable before failing.
    ///
    /// Default: 30
    pub dev_server_timeout_secs: Option<u64>,
    /// Screencast JPEG quality, 1–100.
    ///
    /// Default: 80
    pub screencast_quality: Option<u32>,
    /// Stream only every Nth browser frame.
    ///
    /// Default: 1
    pub screencast_every_nth_frame: Option<u32>,
}
RUST
  ok "settings_content: struct"
fi

insert_after "crates/settings/src/vscode_import.rs" 'audio: None,' \
  '            web_preview: None,' \
  'web_preview: None,' "vscode_import: SettingsContent field"

# ---------------------------------------------------------------------------
# 5. Keymaps — open + pick-mode bindings, all three platforms.
# ---------------------------------------------------------------------------
add_keymap() {
  local file="$1" open_key="$2" pick_key="$3"
  if ! [[ -f "$file" ]]; then warn "keymap: $file not found"; return; fi
  if grep -qF 'web_preview::OpenWebPreview' "$file"; then skip "keymap: $(basename "$file")"; return; fi

  # Insert two context blocks just before the final closing `]` of the top-level array. The default
  # keymaps are a JSON array of context objects; appending two more is valid regardless of upstream
  # churn elsewhere in the file. A trailing comma after the new blocks is fine (the loader is lenient
  # JSONC) and the preceding `}` already has its own comma in these files.
  local blocks
  blocks=$(cat <<EOF
  {
    "context": "Workspace",
    "bindings": { "$open_key": "web_preview::OpenWebPreview" }
  },
  {
    "context": "WebPreview",
    "bindings": { "$pick_key": "web_preview::ToggleWebPickMode" }
  },
EOF
)
  # Find the last line that is just a closing bracket of the top-level array and insert before it.
  local last_bracket
  last_bracket=$(grep -n '^\]' "$file" | tail -1 | cut -d: -f1)
  if [[ -z "$last_bracket" ]]; then
    warn "keymap: $(basename "$file") — couldn't find the array close; add bindings manually"
    return
  fi
  { head -n $((last_bracket - 1)) "$file"; printf '%s\n' "$blocks"; tail -n +"$last_bracket" "$file"; } > "$file.tmp" \
    && mv "$file.tmp" "$file"
  ok "keymap: $(basename "$file")"
}
add_keymap assets/keymaps/default-macos.json   "cmd-k cmd-shift-v"  "cmd-shift-i"
add_keymap assets/keymaps/default-linux.json   "ctrl-k ctrl-shift-v" "ctrl-shift-i"
add_keymap assets/keymaps/default-windows.json "ctrl-k ctrl-shift-v" "ctrl-shift-i"

# ---------------------------------------------------------------------------
# 6. Dev-channel app icon — swap in the Looking Glass icon so a bundled app uses it.
# ---------------------------------------------------------------------------
swap_icon() {
  local src="$1" dst="$2"
  if [[ ! -f "$src" ]]; then warn "icon: source $src missing in the crate"; return; fi
  if [[ -f "$dst.orig" ]]; then skip "icon: $(basename "$dst")"; return; fi
  if [[ -f "$dst" ]]; then cp "$dst" "$dst.orig"; fi
  cp "$src" "$dst"
  ok "icon: $(basename "$dst")"
}
swap_icon crates/web_preview/resources/app-icon.png     crates/zed/resources/app-icon-dev.png
swap_icon crates/web_preview/resources/app-icon@2x.png  crates/zed/resources/app-icon-dev@2x.png

echo
if [[ "$NEEDS_MANUAL" -eq 1 ]]; then
  echo "Some edits need manual attention (see ! lines above). The crate and all clean"
  echo "edits were applied; finish the flagged ones, then run: cargo build -p zed"
  exit 1
else
  echo "Looking Glass re-applied cleanly. Build with: cargo build -p zed"
fi

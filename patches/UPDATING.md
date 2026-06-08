# Updating to a newer Zed — keeping Looking Glass intact

This fork carries one feature on top of Zed: **Looking Glass** (the `web_preview`
crate plus a handful of small edits to Zed-owned files). We use a **patch-on-top**
model rather than a tracking merge, because it keeps the feature isolated and the
update process low-risk: our changes are re-applied on top of upstream, never
entangled with it.

## What "our changes" actually are

- **New, self-contained:** `crates/web_preview/` (the entire feature — never conflicts).
- **Small edits to Zed files** (the only possible conflict points):
  - `Cargo.toml` — workspace member + dependency (2 lines)
  - `crates/zed/Cargo.toml` — dependency (1 line)
  - `crates/zed/src/main.rs` — `web_preview::init(...)` (1 line)
  - `crates/settings_content/src/settings_content.rs` — one field + one struct
  - `crates/settings/src/vscode_import.rs` — one field (1 line)
  - `assets/keymaps/default-{macos,linux,windows}.json` — two bindings each
  - `crates/zed/resources/app-icon-dev.png` + `@2x.png` — our icon (originals: `*.orig`)

That is the complete footprint. Everything else is stock Zed.

## How to take a Zed update

The clean way (a throwaway working copy, so nothing risks the installed app):

```sh
# 1. Get the latest upstream Zed into a fresh directory.
git clone https://github.com/zed-industries/zed.git zed-fresh
cd zed-fresh

# 2. Copy our patch kit in.
mkdir -p patches
cp ../zed-main/patches/looking-glass-crate.tar.gz patches/
cp ../zed-main/patches/apply-looking-glass.sh     patches/

# 3. Re-apply Looking Glass on top of the new Zed.
bash patches/apply-looking-glass.sh

# 4. Build + install.
cargo build -p zed                 # to run from source, or:
script/bundle-mac -i               # to reinstall the "Zed Dev.app" with our icon
```

`apply-looking-glass.sh` is **idempotent and honest**:
- It skips edits that are already present (safe to re-run).
- If upstream moved an anchor it relies on, it **stops and tells you exactly which
  edit needs a manual touch** instead of silently corrupting a file.

## When the script flags an edit

That means Zed restructured a file we edit (e.g. they reorganized
`main.rs`'s init block). Open that file, add our one line by hand near the same
area (the script prints what to add), then continue. This is rare and small —
our edits are one or two lines in stable spots.

## Re-generating the crate tarball

If you change the `web_preview` crate itself, refresh the portable copy:

```sh
tar -czf patches/looking-glass-crate.tar.gz crates/web_preview/
```

## Why not a git merge with upstream?

This snapshot's version markers don't line up with any real Zed release
(workspace `0.61`, `crates/zed` `1.7.0` vs. upstream `v0.18x`), so it has no
clean shared history to merge against. Patch-on-top sidesteps that entirely and
keeps the feature portable to *any* future Zed version.

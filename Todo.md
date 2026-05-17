# TODO — Native Theme System + Styled Config Editor / Settings Panel

**Status: All phases implemented. See details below.**

---

## Phase 1 — Shared Native Theme Module ✅

- `mhd-daemon/src/native_theme.rs`
- `Argb` with hex parse, COLORREF, premultiplied pixel helper
- `NativeTheme` struct with all colour fields + Default (built-in dark)
- JSON deserialization of Zed-compatible theme files
- `themes_dir()` → `%USERPROFILE%\.config\mhd\themes`
- `load_theme(Option<&str>)` → `NativeTheme` (silent fallback on errors)
- `load_theme_from_path()` made `pub` for settings panel reuse
- 9 unit tests, all passing

## Phase 2 — Store Theme in App/AppHandle ✅

- `AppHandle.theme: Arc<Mutex<NativeTheme>>`
- `App.theme: Arc<Mutex<NativeTheme>>`
- `AppHandle::theme()` getter
- `AppHandle::reload_config()` reloads theme from new config
- On reload: theme → OSD via `self.osd.set_theme()`
- Initial theme pushed to OSD in `main.rs`

## Phase 3 — Theme the OSD ✅

- `OsdCommand::SetTheme(NativeTheme)` command
- `OsdHandle::set_theme()` method
- OSD thread stores local `theme: NativeTheme`
- `paint_osd()` accepts `&NativeTheme`, uses theme colors for:
  - Background (rounded rect)
  - Text colour
  - "Brightness" label (muted)
  - Progress bar track/fill
- `draw_rounded_rect()` accepts `Argb` color parameter

## Phase 4 — Theme the About Dialog ✅

- `show_about(theme: NativeTheme)` accepts theme parameter
- All colours sourced from theme (background, text, muted, border, etc.)
- Tray passes `state.app.theme()` to About

## Phase 5 — Shared Native UI Helpers 🔲

Skipped (optional). `to_utf16_z` and `draw_rounded_rect` remain public in `osd.rs`.

## Phase 6 — Styled Settings Panel ✅

**Rewritten twice based on user feedback:**

### v1 — Raw text editor (removed)
- Native multiline `EDIT` control with dark theme
- Ctrl+S / Ctrl+Enter / Esc shortcuts
- TOML validation before save
- ❌ User rejected: "не блокнот"

### v2 — Structured Settings Panel (current)
- Fully layered `WS_EX_LAYERED` + `UpdateLayeredWindow` with per-pixel alpha
- All controls drawn manually via GDI on DIB (no child HWNDs)
- **Draggable** via `WM_NCHITTEST` returning `HTCAPTION` for header area
- **Theme selector**: custom combo box drawn on DIB
  - Dropdown list is a second layered popup with per-pixel alpha
  - Click item → immediately applies and closes popup
- **Apply button**: writes `theme = "..."` to `config.toml`, reloads daemon
- **Glass themes work correctly**: semi-transparent colours render with alpha
- **Opens multiple times**: `RegisterClassW` failure ignored (class may already exist)
- Architecture extensible: add new setting rows by copying the Theme pattern

## Phase 7 — Build & Validation ✅

- `cargo check` — no errors, ~45 warnings (all `unsafe_op_in_unsafe_fn` — Rust 2024)
- `cargo build --release` — success
- `cargo test native_theme` — 9/9 pass
- Binary: `target/release-tmp/release/mhd.exe`

## Phase 8 — README Update ✅

- Documented native theme system (file location, supported color keys)
- Documented settings panel, About dialog, OSD
- Documented architecture overview

---

## Remaining / Future Ideas

- More settings: brightness steps, volume OSD toggle, startup behaviour
- Theme preview swatch in settings panel
- Keyboard navigation in combo box (arrow keys)
- Styled confirm dialog (replace `MessageBoxW`)
- Config export/import
- `theme.name` field dead code — used only in combo box display, could be cleaned

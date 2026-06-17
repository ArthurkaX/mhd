# Release Process

This project publishes portable Windows x64 archives.

## Build Package

From the repository root:

```powershell
.\scripts\package-release.ps1 -Version 0.4.0
```

The script builds the default public binary without optional developer features and writes:

```text
dist\mhd-v0.4.0-windows-x64.zip
dist\mhd-v0.4.0-windows-x64.zip.sha256
```

## GitHub Release

1. Make sure `CHANGELOG.md` has the release version and date.
2. Commit release documentation and packaging changes.
3. Create and push the tag:

   ```powershell
   git tag v0.4.0
   git push origin main
   git push origin v0.4.0
   ```

4. Create a GitHub Release for `v0.4.0`.
5. Upload both generated files from `dist`.

## Release Notes Template

```markdown
## mHD 0.4.0

This release adds the **KeyCast** keystroke overlay — a visualizer that shows pressed keys, mouse clicks, and modifier combos in a carousel of rounded pills. It is designed for recordings, streams, and screencasts where viewers need to see what keys are being pressed.

### Highlights

- **KeyCast overlay** — rounded pill carousel shows pressed shortcuts (e.g. `Ctrl+Alt+T`), mouse clicks, and modifier combos with smooth enter/exit animations.
- **Typing block** (optional, off by default) — single printable characters (letters, digits, space) appear in a fixed-width block with a mini-carousel animation. Characters are resolved via `ToUnicodeEx` against the **foreground window's keyboard layout**, so Russian, English, and other layouts show the correct glyph.
- **Configurable layout** — choose between 6 positions (top/bottom × left/center/right) and adjust display duration from Settings → General or `[keycast]` in config.
- **Tray integration** — toggle KeyCast on/off from the tray menu, or bind `toggle_keycast` to any hotkey.
- **Tray clarification** — "mhd on/off" renamed to "Key Shortcuts on/off" to make clear it suspends only hotkey processing without stopping the daemon.

### Documentation

- Added KeyCast configuration docs to README (actions, settings, TOML config keys).
- Updated editor action list with `toggle_keycast`.

### Fixes

- Settings editor General page scroll area now correctly matches the expanded layout — typing controls no longer clip below the scroll region.

### New files

- `mhd-daemon/src/overlays/keycast.rs`

### Package

- Windows x64 portable archive
- No installer required
- No WebView2 required
- Built without optional developer-only `blackbox`

### Verification

SHA256 is provided as a separate `.sha256` file.

### Note

The binary is currently unsigned, so Windows SmartScreen may show an unknown publisher warning on first launch.
```

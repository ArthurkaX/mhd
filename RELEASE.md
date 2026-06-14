# Release Process

This project publishes portable Windows x64 archives.

## Build Package

From the repository root:

```powershell
.\scripts\package-release.ps1 -Version 0.3.0
```

The script builds the default public binary without optional developer features and writes:

```text
dist\mhd-v0.3.0-windows-x64.zip
dist\mhd-v0.3.0-windows-x64.zip.sha256
```

## GitHub Release

1. Make sure `CHANGELOG.md` has the release version and date.
2. Commit release documentation and packaging changes.
3. Create and push the tag:

   ```powershell
   git tag v0.3.0
   git push origin main
   git push origin v0.3.0
   ```

4. Create a GitHub Release for `v0.3.0`.
5. Upload both generated files from `dist`.

## Release Notes Template

```markdown
## mHD 0.3.0

Release of mHD, a lightweight native Windows tray daemon for hotkeys, remaps, monitor control, audio control, quick notes, timers, LLM proxy routing, and small desktop tools.

### Package

- Windows x64 portable archive
- No installer required
- No WebView2 required
- Built without optional developer-only `blackbox`

### Install

Extract the archive and run `mhd.exe`.

Config is created at:

`%USERPROFILE%\.config\mhd\config.toml`

### Verification

SHA256 is provided as a separate `.sha256` file.

### Note

The binary is currently unsigned, so Windows SmartScreen may show an unknown publisher warning on first launch.
```

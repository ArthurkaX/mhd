# Release Process

This project publishes portable Windows x64 archives.

## Build Package

From the repository root:

```powershell
.\scripts\package-release.ps1 -Version 0.8.0
```

The script builds the public binaries without optional developer features and writes:

```text
dist\mhd-v0.8.0-windows-x64.zip
dist\mhd-v0.8.0-windows-x64.zip.sha256
```

The archive contains `mhd.exe`, `mhd-inspector.exe`, `LICENSE`, `INSTALL.md`,
`README.md`, and `claude-mhd.bat`.

## GitHub Release

1. Make sure `CHANGELOG.md` has the release version and date.
2. Commit release documentation and packaging changes.
3. Create and push the tag:

   ```powershell
   git tag v0.8.0
   git push origin main
   git push origin v0.8.0
   ```

4. Create a GitHub Release for `v0.8.0`, using the `0.8.0` changelog entry.
5. Upload both generated files from `dist`.

## Release Notes — mHD 0.8.0

This release expands mHD's LLM workflow with native Codex routing, quota
visibility, and a standalone request inspector, while adding quiet power mode
and direct sleep/hibernate actions.

### Highlights

- **Native Codex proxy** — OAuth-backed Responses and WebSocket support,
  side-model routing, and a separate Codex model selector.
- **Codex request trim** — conservative, fail-open tool-output compression with
  provenance and content gates, diagnostics handling, stale-image cleanup, and
  native-style head/tail budgets.
- **Quota visibility** — live Codex and Anthropic quota tracking, timeline notes,
  pace summaries, and quota charts in the tray and inspector.
- **LLM Monitor** — standalone `mhd-inspector.exe` shipped alongside `mhd.exe`,
  with request activity, trim, and quota views.
- **Quiet mode and power actions** — display-off quiet mode keeps the machine
  awake with capped CPU usage, plus bindable sleep and hibernate actions.
- **Proxy reliability** — improved cache accounting, route attribution,
  database repair, UTF-8 cursor handling, and model-list refresh behavior.

### Package

- Windows x64 portable archive
- No installer required
- No WebView2 required
- Built without optional developer-only `blackbox`

### Verification

SHA256 is provided as a separate `.sha256` file.

### Note

The binaries are currently unsigned, so Windows SmartScreen may show an
unknown publisher warning on first launch.

# mHD Install

mHD is distributed as a portable Windows executable.

## Install

1. Download the latest `mhd-v*-windows-x64.zip` from GitHub Releases.
2. Extract it to a folder you control, for example:

   ```text
   C:\Tools\mhd
   ```

3. Run:

   ```powershell
   .\mhd.exe
   ```

The app starts as a tray daemon. On first run it creates the config at:

```text
%USERPROFILE%\.config\mhd\config.toml
```

## LLM Proxy Launcher

The archive also includes `claude-mhd.bat` — a batch wrapper to launch Claude Code through mHD's built-in LLM proxy. To use it from any terminal, add the extraction folder to your `PATH` or copy the `.bat` to a folder already on your `PATH`. Then run:

```powershell
claude-mhd                          # default model
claude-mhd --model sonnet -p "hi"   # pick model and prompt
```

See `claude-mhd.bat` for instructions depending on whether you have an Anthropic subscription.

## Autostart

Open the tray menu and enable autostart from mHD. The app will try to register a Windows logon task and falls back to the current-user Run key when needed.

If Windows blocks the first launch because the binary is unsigned, choose the usual "More info" / "Run anyway" flow after verifying that the file came from the project release page.

## Public Build

The distributed binary is built without optional developer features:

```powershell
cargo build --release --no-default-features
```

The `blackbox` feature is not included in normal release archives. It exists only for users who intentionally build mHD from source with:

```powershell
cargo build --release --features blackbox
```

## Verify Download

Each release archive should include a SHA256 checksum next to the zip file on the release page. Verify it with:

```powershell
Get-FileHash .\mhd-v0.8.0-windows-x64.zip -Algorithm SHA256
```

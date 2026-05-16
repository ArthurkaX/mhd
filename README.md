# mhd — Mouse & Hotkey Daemon for Windows

**mhd** is a lightweight background daemon that remaps keys, mouse buttons, and keyboard shortcuts using low-level Windows hooks. No drivers, no kernel components — just two portable `.exe` files.

Think of it as a programmable `AutoHotkey`-lite that runs silently in the background, driven by a simple TOML config file, with a tiny tray icon for control.

---

## Final Vision (target state)

```
┌─────────────────────────────────────────────────────────────────┐
│                        mhd  (end-state)                          │
│                                                                  │
│   ┌──────────┐     IPC (named pipe)     ┌──────────────────┐    │
│   │ mhd.exe  │◄───────────────────────►│  mhd-tray.exe    │    │
│   │ (daemon) │                          │  (tray UI)       │    │
│   │          │                          │                  │    │
│   │ • hooks  │                          │  🟢 tray icon    │    │
│   │ • config │                          │  ┌────────────┐  │    │
│   │ • actions│                          │  │Status: OK   │  │    │
│   │ • no GUI │                          │  │Edit Config  │  │    │
│   └──────────┘                          │  │Reload Config│  │    │
│                                         │  │Restart      │  │    │
│   User runs:                            │  │Quit mhd     │  │    │
│   mhd-tray.exe                          │  └────────────┘  │    │
│   → auto-starts daemon                  └──────────────────┘    │
│   → right-click tray icon                                      │
│     to manage everything                                        │
└─────────────────────────────────────────────────────────────────┘
```

**User experience:**
1. Download two files: `mhd.exe` and `mhd-tray.exe` (plus `mHD_32.png` icon)
2. Put them in any folder
3. Launch `mhd-tray.exe` — it auto-starts the daemon and shows a tray icon
4. Right-click the tray icon to edit config, reload, restart, or quit
5. Config lives at `%USERPROFILE%\.config\mhd\config.toml` (auto-created on first run)
6. Add `mhd-tray.exe` to Windows Startup for auto-launch at login

---

## Features

- **Key remapping** — replace any key or shortcut with another (e.g. `CapsLock` → `Alt+Shift`, or `MouseButton4` → `Ctrl+Win+Left`)
- **Mouse button bindings** — bind side buttons (XButton1/XButton2, a.k.a. "Mouse4"/"Mouse5") to keyboard shortcuts or PowerShell commands
- **Run arbitrary PowerShell** — execute any PowerShell command on a hotkey
- **Monitor brightness control** — adjust monitor brightness directly via DDC/CI (no external tools needed)
- **Scheme switching** — define multiple layers of bindings and switch between them at runtime
- **Tray icon** — see daemon status at a glance, manage everything from the context menu
- **Low-level hooks** — uses `WH_KEYBOARD_LL` / `WH_MOUSE_LL`, works in most applications (games may behave differently)
- **Portable** — two small binaries, no installation, no admin rights required (for most features)

---

## Quick Start

### 1. Install Rust (if building from source)

```powershell
# Install Rust via winget
winget install Rustlang.Rustup
```

### 2. Build

```powershell
cd mhd
cargo build --release
```

Output binaries:

| File | Purpose |
|---|---|
| `target\release\mhd.exe` | Daemon (hooks, config, actions) |
| `target\release\mhd-tray.exe` | Tray UI (status, menu, manages daemon) |

Copy both `.exe` files and `mhd-ui\mHD_32.png` into the same folder.

### 3. First run

```powershell
# Launch the tray (it auto-starts the daemon)
.\mhd-tray.exe
```

A tray icon appears. The daemon is started automatically. On first run, a default config is created at:

```
%USERPROFILE%\.config\mhd\config.toml
```

### 4. Configure

Right-click the tray icon → **Edit Config**. Uncomment the bindings you want, save, then right-click → **Reload Config**.

Or run the daemon standalone (no tray):

```powershell
.\mhd.exe --quiet
```

---

## Architecture

mhd is split into two programs that communicate via a named pipe:

### `mhd.exe` — Daemon

- Reads `%USERPROFILE%\.config\mhd\config.toml`
- Installs `WH_KEYBOARD_LL` + `WH_MOUSE_LL` hooks
- Matches key/mouse events against configured triggers
- Executes actions: key replacement (`SendInput`), PowerShell commands, monitor brightness (DDC/CI)
- Runs an IPC server on `\\.\pipe\mhd_ipc_pipe`
- Headless — no windows, no tray icon
- CLI: `--quiet` suppresses startup log lines

### `mhd-tray.exe` — Tray UI

- Shows a tray icon (green = daemon running, red = stopped)
- Right-click context menu:
  - **Status** — shows "running" or "stopped"
  - **Edit Config** — opens `config.toml` in default editor
  - **Reload Config** — tells daemon to re-read config without restart
  - **Restart Daemon** — kills and re-launches `mhd.exe`
  - **Quit mhd** — shuts down both daemon and tray
- Auto-starts `mhd.exe` on launch
- Only one instance allowed at a time

### IPC Protocol

Channel: `\\.\pipe\mhd_ipc_pipe`

| Command (UI → Daemon) | Response | Action |
|---|---|---|
| `status` | `running\n` | Health check |
| `reload` | `reloading\n` | Re-read `config.toml` |
| `shutdown` | `shutting_down\n` | Graceful exit |

---

## Configuration

### Config location

| Environment variable | Path |
|---|---|
| `MHD_CONFIG` (set) | `%MHD_CONFIG%` |
| `MHD_CONFIG` (unset) | `%USERPROFILE%\.config\mhd\config.toml` |

### Format

The config is [TOML](https://toml.io). Each hotkey is a `[[binding]]` block:

```toml
# Optional: startup scheme (defaults to "default")
active_scheme = "default"

[[binding]]
trigger = "ctrl+alt+f12"     # the key/mouse combination to listen for
action = "quit"              # what to do

[[binding]]
trigger = "capslock"
action = "replace_key"
keys = "alt+shift"
```

---

## Actions

### `quit`

Exit the daemon.

```toml
[[binding]]
trigger = "ctrl+alt+f12"
action = "quit"
```

### `replace_key`

When the trigger is pressed, suppress it and send a different key combination instead.

```toml
# Replace CapsLock with Alt+Shift (useful for keyboard layout switching)
[[binding]]
trigger = "capslock"
action = "replace_key"
keys = "alt+shift"

# Map mouse side button 4 to Ctrl+Win+Left (switch virtual desktop left)
[[binding]]
trigger = "mouseButton4"
action = "replace_key"
keys = "ctrl+win+left"

# Map mouse side button 5 to Ctrl+Win+Right (switch virtual desktop right)
[[binding]]
trigger = "mouseButton5"
action = "replace_key"
keys = "ctrl+win+right"
```

**Important:** `replace_key` works with keyboard keys and mouse button *triggers*, but the emitted `keys` value must be a keyboard combination (mouse buttons cannot be emitted via `SendInput` in this way).

### `run_ps`

Run an arbitrary PowerShell command. The command is executed via `powershell -Command ...` (profile is loaded, so modules are available).

```toml
# Open a specific website
[[binding]]
trigger = "ctrl+alt+b"
action = "run_ps"
command = "Start-Process 'https://github.com'"
```

> **Tip:** For virtual desktop switching, prefer `replace_key` with `ctrl+win+left`/`ctrl+win+right` — it's faster and requires no PowerShell module.

### `set_brightness`

Change the primary monitor's brightness through DDC/CI. Your monitor must support DDC/CI (almost all modern monitors do; it may need to be enabled in the monitor's OSD menu).

```toml
# Increase brightness by 5%
[[binding]]
trigger = "ctrl+alt+numpad_add"
action = "set_brightness"
value = "+5"

# Decrease brightness by 5%
[[binding]]
trigger = "ctrl+alt+numpad_subtract"
action = "set_brightness"
value = "-5"

# Set brightness to exactly 50%
[[binding]]
trigger = "ctrl+alt+0"
action = "set_brightness"
value = "50"
```

- `"+N"` / `"-N"` — relative adjustment (clamped to 0–100)
- `"50"` — absolute value (0–100)

> **Note:** This uses the Win32 Monitor Configuration API (`dxva2.dll`). No third‑party tools are needed. Only the primary monitor is controlled.

### `switch_scheme`

Switch to a different binding scheme. Schemes allow you to have multiple layers of hotkeys and toggle between them.

```toml
active_scheme = "default"

[[binding]]
trigger = "ctrl+shift+s"
action = "switch_scheme"
target_scheme = "gaming"
scheme = "default"           # this binding only active in "default" scheme

# --- Default scheme bindings ---

[[binding]]
trigger = "f1"
action = "run_ps"
command = "Write-Host 'Normal mode F1'"
scheme = "default"

# --- Gaming scheme bindings ---

[[binding]]
trigger = "f1"
action = "run_ps"
command = "Write-Host 'Gaming mode F1'"
scheme = "gaming"
```

- Each binding belongs to a `scheme` (defaults to `"default"`)
- `switch_scheme` changes the active scheme for all future key presses
- Only bindings in the active scheme are active

---

## Trigger Syntax

Triggers are case-insensitive strings of `modifiers + key`, joined by `+`:

```
trigger = "ctrl+alt+f12"
```

### Modifiers

| Name | Notes |
|---|---|
| `alt` | |
| `ctrl` | Also accepts `control` |
| `shift` | |
| `win` | Also accepts `super` |

### Keys

#### Letters & Digits

| Syntax | Examples |
|---|---|
| Single letter | `a`, `b`, …, `z` |
| Single digit | `0`, `1`, …, `9` |

#### Function Keys

| Syntax | Examples |
|---|---|
| `f1` … `f24` | `f1`, `f12` |

#### Named Keys

| Name | Aliases |
|---|---|
| `capslock` | `capital` |
| `space` | |
| `tab` | |
| `enter` | `return` |
| `esc` | `escape` |
| `backspace` | |
| `delete` | `del` |
| `insert` | `ins` |
| `home` | |
| `end` | |
| `pageup` | `prior` |
| `pagedown` | `next` |
| `left` | |
| `right` | |
| `up` | |
| `down` | |
| `printscreen` | |
| `scrolllock` | |
| `numlock` | |
| `contextmenu` | `apps` |

#### Numpad

| Name | Aliases |
|---|---|
| `numpad0` … `numpad9` | |
| `numpadadd` | `numpad_plus`, `numpad_add` |
| `numpadsubtract` | `numpad_minus`, `numpad_subtract` |
| `numpadmultiply` | `numpad_star` |
| `numpaddivide` | `numpad_slash` |
| `numpadenter` | |
| `numpaddecimal` | `numpad_dot` |

#### OEM / Punctuation

| Name | Aliases |
|---|---|
| `minus` | `oem_minus` |
| `equal` | `oem_equal`, `equals` |
| `comma` | `oem_comma` |
| `period` | `oem_period` |
| `slash` | `oem_slash` |
| `semicolon` | `oem_semicolon` |
| `quote` | `oem_quote` |
| `backslash` | `oem_backslash` |
| `lbracket` | `oem_lbracket`, `oem_4` |
| `rbracket` | `oem_rbracket`, `oem_6` |
| `backquote` | `oem_3`, `grave` |

#### Media Keys

| Name |
|---|
| `volume_mute` |
| `volume_down` |
| `volume_up` |
| `media_next` |
| `media_prev` |
| `media_stop` |
| `media_play_pause` |

#### Mouse Buttons

| Name | Maps to |
|---|---|
| `mouseButton4` | XButton1 (side button, usually "back") |
| `mouseButton5` | XButton2 (side button, usually "forward") |

---

## Complete Example Config

```toml
active_scheme = "default"

# ── Always-active bindings ───────────────────────────────────────────

[[binding]]
trigger = "ctrl+alt+f12"
action = "quit"

# ── Keyboard layout switching ────────────────────────────────────────

[[binding]]
trigger = "capslock"
action = "replace_key"
keys = "alt+shift"

# ── Virtual desktop navigation (mouse side buttons) ──────────────────

[[binding]]
trigger = "mouseButton4"
action = "replace_key"
keys = "ctrl+win+left"

[[binding]]
trigger = "mouseButton5"
action = "replace_key"
keys = "ctrl+win+right"

# ── Monitor brightness ───────────────────────────────────────────────

[[binding]]
trigger = "ctrl+alt+numpad_add"
action = "set_brightness"
value = "+5"

[[binding]]
trigger = "ctrl+alt+numpad_subtract"
action = "set_brightness"
value = "-5"

# ── Quick launcher ────────────────────────────────────────────────────

[[binding]]
trigger = "ctrl+alt+t"
action = "run_ps"
command = "Start-Process wt"
```

---

## Command-Line Options (Daemon)

| Flag | Description |
|---|---|
| *(none)* | Run the daemon (normal mode) |
| `--quiet` | Suppress startup log lines; only errors are printed |
| `MHD_CONFIG=path` | Override config file path (environment variable) |

The tray UI (`mhd-tray.exe`) has no command-line flags — it always runs with `#![windows_subsystem = "windows"]` (no console window).

---

## Auto-Start with Windows

1. Press `Win+R`, type `shell:startup`, press Enter
2. Create a shortcut to `mhd-tray.exe` (not `mhd.exe` directly)
3. The tray will auto-start the daemon on login

Alternatively, use Task Scheduler to start `mhd-tray.exe` at logon with highest available privileges (some features like brightness may require this on certain systems).

---

## System Requirements

- **Windows 10** or **Windows 11** (x86_64)
- For `set_brightness`: a monitor that supports **DDC/CI** (almost all modern monitors; may need to be enabled in the OSD under "DDC/CI" or similar)
- For `run_ps`: **PowerShell 5.1** or later (built into Windows)

---

## How It Works (Internals)

```
                    ┌──────────────────────────────┐
                    │         User launches          │
                    │        mhd-tray.exe            │
                    └──────────────┬─────────────────┘
                                   │
                    ┌──────────────▼─────────────────┐
                    │   Tray UI (mhd-tray.exe)        │
                    │                                 │
                    │ • Creates hidden window         │
                    │ • Loads tray icon from PNG      │
                    │ • Start mhd.exe via              │
                    │   CreateProcessW                │
                    │ • Right-click → popup menu      │
                    └──────────────┬─────────────────┘
                                   │ named pipe
                    ┌──────────────▼─────────────────┐
                    │   Daemon (mhd.exe)              │
                    │                                 │
                    │  ┌─────────────────────────┐    │
                    │  │       IPC thread         │    │
                    │  │  \\.\pipe\mhd_ipc_pipe   │    │
                    │  └─────────────────────────┘    │
                    │                                 │
                    │  ┌─────────────────────────┐    │
                    │  │    Main thread (hooks)   │    │
                    │  │  WH_KEYBOARD_LL          │    │
                    │  │  WH_MOUSE_LL             │    │
                    │  │       ↓                  │    │
                    │  │  Trigger match?          │    │
                    │  │       ↓                  │    │
                    │  │  ActionMessage →          │    │
                    │  └──────────┬──────────────┘    │
                    │             ↓                   │
                    │  ┌─────────────────────────┐    │
                    │  │    Worker thread         │    │
                    │  │                          │    │
                    │  │  replace_key → SendInput │    │
                    │  │  run_ps     → powershell │    │
                    │  │  brightness → dxva2.dll  │    │
                    │  │  quit       → PostQuit   │    │
                    │  └─────────────────────────┘    │
                    └─────────────────────────────────┘
```

1. **Tray UI** (`mhd-tray.exe`) launches the daemon and shows a tray icon
2. **Daemon** (`mhd.exe`) installs low-level hooks, starts an IPC thread, and a worker thread
3. **Hook callbacks** match key/mouse events against the trigger map of the active scheme
4. Matched events are swallowed and sent as `ActionMessage` to the worker thread
5. **Worker thread** executes the action: `SendInput` for key replacement, `powershell` spawning for `run_ps`, or Monitor Configuration API calls for brightness
6. **IPC thread** listens on the named pipe for commands from the tray UI (status, reload, shutdown)

### Safety measures

- **Injected-event guard:** events generated by `SendInput` are skipped to prevent infinite loops
- **Key-up tracking:** when a key-down is swallowed, the corresponding key-up is also swallowed to keep modifier state consistent
- **Thread isolation:** hook callbacks are minimal (lock → lookup → send message → return); all side effects run on the worker thread

---

## Troubleshooting

### "config empty" on startup
Uncomment at least one `[[binding]]` in the config file. The daemon refuses to start with zero active bindings.

### "no physical monitors found" (brightness)
Your monitor may not be exposing DDC/CI. Check your monitor's OSD menu for a "DDC/CI" option and enable it. Some laptop internal displays do not support DDC/CI.

### Brightness doesn't change
Try running mhd as **Administrator** — on some systems the Monitor Configuration API requires elevation.

### `mhd.exe` already running
Only one instance can run at a time. Close the existing one (via tray icon → Quit, or `Ctrl+Alt+F12`) or kill it via Task Manager.

### Tray icon not showing
Make sure `mHD_32.png` is in the same folder as `mhd-tray.exe`.

### Game compatibility
Low-level hooks (`WH_KEYBOARD_LL`) may be ignored or behave differently in full-screen games that use raw input. This is a Windows limitation.

---

## Building & Development

```powershell
# Build everything (both daemon + tray UI)
cargo build --release

# Build only the daemon
cargo build --release -p mhd-daemon

# Build only the tray UI
cargo build --release -p mhd-ui

# Run tests (none yet)
cargo test
```

### Project Structure

```
mhd/
├── Cargo.toml              # workspace root
├── README.md
├── TODO.md
├── icons/
│   ├── mHD_16.png          # tray icon 16×16
│   ├── mHD_32.png          # tray icon 32×32 (loaded by mhd-tray)
│   ├── mHD_256.png         # source icon 256×256
│   ├── generate.ps1        # generates 16/32 from 256
│   └── build_icon.ps1      # generates .ico (WIP)
├── mhd-daemon/
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs          # Entry point, CLI args, spawns IPC + hooks
│       ├── action.rs        # Action enum, parsing & validation
│       ├── brightness.rs    # DDC/CI monitor brightness via dxva2.dll
│       ├── config.rs        # TOML parsing, validation, scheme management
│       ├── hook.rs          # WH_KEYBOARD_LL + WH_MOUSE_LL, message loop
│       ├── ipc.rs           # Named pipe server (status/reload/shutdown)
│       ├── trigger.rs       # Trigger/key parsing, modifier detection
│       └── worker.rs        # Action execution (SendInput, PowerShell, DDC/CI)
└── mhd-ui/
    ├── Cargo.toml
    ├── mHD_32.png           # Tray icon (loaded at runtime by LoadImageW)
    └── src/
        └── main.rs          # Tray-only UI, context menu, daemon lifecycle
```

---

## License

MIT or Apache-2.0 (at your option).

---

## Alternatives & Inspiration

- [AutoHotkey](https://www.autohotkey.com/) — much more powerful scripting, but a larger runtime
- [PowerToys Keyboard Manager](https://learn.microsoft.com/en-us/windows/powertoys/keyboard-manager) — simple remapping, no mouse buttons
- [kanata](https://github.com/jtroo/kanata) — advanced key remapper with layers, cross-platform but does not support mouse buttons as triggers on Windows

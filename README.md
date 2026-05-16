# mhd — Mouse & Hotkey Daemon for Windows

**mhd** is a lightweight background daemon that remaps keys, mouse buttons, and keyboard shortcuts using low-level Windows hooks. No drivers, no kernel components — just one portable `.exe` file.

Think of it as a programmable `AutoHotkey`-lite that runs silently in the background, driven by a simple TOML config file, with a tray icon for control.

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        mhd.exe (single binary)                  │
│                                                                 │
│   ┌─────────────────────────────────────────────────────────┐   │
│   │                     Daemon Core                         │   │
│   │                                                         │   │
│   │  • WH_KEYBOARD_LL / WH_MOUSE_LL hooks                   │   │
│   │  • TOML config management                               │   │
│   │  • Action execution (SendInput, PowerShell, DDC/CI)     │   │
│   │  • IPC server (named pipe for external control)         │   │
│   └───────────────▲───────────────────────────▲─────────────┘   │
│                   │                           │                 │
│         Direct internal calls           Direct internal calls   │
│                   │                           │                 │
│   ┌───────────────▼──────────────┐   ┌────────▼─────────────┐   │
│   │         Tray Module          │   │      IPC Server      │   │
│   │                              │   │                      │   │
│   │  🟢 Tray Icon                │   │  • status            │   │
│   │  ┌────────────┐              │   │  • reload            │   │
│   │  │Status: OK   │              │   │  • shutdown          │   │
│   │  │Edit Config  │              │   └──────────────────────┘   │
│   │  │Reload Config│              │                             │
│   │  │Quit mhd     │              │     (headless control)      │
│   │  └────────────┘              │                             │
│   └──────────────────────────────┘                             │
│                                                                 │
│   User runs:                                                    │
│   mhd.exe                                                       │
│   → starts daemon + tray (default)                              │
│                                                                 │
│   mhd.exe --daemon                                              │
│   → starts daemon headless (no tray)                            │
└─────────────────────────────────────────────────────────────────┘
```

**User experience:**
1. Download `mhd.exe` (and optionally `mHD_32.png` for the tray icon)
2. Put them in any folder
3. Launch `mhd.exe` — it starts the daemon and shows a tray icon
4. Right-click the tray icon to edit config, reload, or quit
5. Config lives at `%USERPROFILE%\.config\mhd\config.toml` (auto-created on first run)
6. Add `mhd.exe` to Windows Startup for auto-launch at login

---

## Features

- **Key remapping** — replace any key or shortcut with another (e.g. `CapsLock` → `Alt+Shift`, or `MouseButton4` → `Ctrl+Win+Left`)
- **Mouse button bindings** — bind side buttons (XButton1/XButton2, a.k.a. "Mouse4"/"Mouse5") to keyboard shortcuts or PowerShell commands
- **Run arbitrary PowerShell** — execute any PowerShell command on a hotkey
- **Monitor brightness control** — adjust monitor brightness directly via DDC/CI (no external tools needed)
- **Scheme switching** — define multiple layers of bindings and switch between them at runtime
- **Tray icon** — see daemon status at a glance, manage everything from the context menu
- **Low-level hooks** — uses `WH_KEYBOARD_LL` / `WH_MOUSE_LL`, works in most applications
- **Portable** — single small binary, no installation, no admin rights required (for most features)

---

## Quick Start

### 1. Build from source

```powershell
cd mhd
cargo build --release
```

Output binary: `target\release\mhd.exe`

### 2. Run

```powershell
# Launch (with tray icon)
.\mhd.exe
```

A tray icon appears. On first run, a default config is created at:
`%USERPROFILE%\.config\mhd\config.toml`

### 3. Configure

Right-click the tray icon → **Edit Config**. Uncomment the bindings you want, save, then right-click → **Reload Config**.

---

## Actions

### `quit`
Exit the program.

### `replace_key`
Suppress the trigger and send a different key combination.
- `trigger = "capslock"`, `action = "replace_key"`, `keys = "alt+shift"`
- `trigger = "mouseButton4"`, `action = "replace_key"`, `keys = "ctrl+win+left"`

### `run_ps`
Run a PowerShell command.
- `command = "Start-Process wt"`

### `set_brightness`
Adjust monitor brightness via DDC/CI.
- `value = "+5"` (increase)
- `value = "-5"` (decrease)
- `value = "50"` (absolute)

### `switch_scheme`
Switch binding layers.

---

## Command-Line Options

| Flag | Description |
|---|---|
| *(none)* | Run with tray icon |
| `--daemon` | Run headless (no tray icon) |
| `--quiet` | Suppress startup log lines |
| `--help` | Show help |

---

## Troubleshooting

- **"config empty"**: Uncomment at least one binding.
- **Brightness issues**: Ensure DDC/CI is enabled in monitor OSD. May require Administrator rights.
- **Tray icon missing**: Ensure `mHD_32.png` is next to `mhd.exe`.

---

## Internals

1. **Main thread** runs the tray UI or hook loop.
2. **Hook thread** (if in tray mode) handles `WH_KEYBOARD_LL` / `WH_MOUSE_LL`.
3. **Worker thread** executes actions to keep hooks responsive.
4. **IPC thread** allows external control via named pipe.

---

## Project Structure

```
mhd/
├── Cargo.toml          # workspace root
├── icons/              # source icons and generation scripts
├── mhd-daemon/         # main crate (produces mhd.exe)
│   ├── src/
│   │   ├── main.rs     # Entry & CLI
│   │   ├── app.rs      # App orchestration
│   │   ├── tray.rs     # Tray UI module
│   │   ├── hook.rs     # Windows hooks
│   │   └── ...
└── README.md
```

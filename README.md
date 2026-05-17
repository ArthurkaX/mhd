# mhd — Mouse & Hotkey Daemon for Windows

**mhd** is a lightweight background daemon that remaps keys, mouse buttons,
and keyboard shortcuts using low‑level Windows hooks. No drivers, no kernel
components — just one portable `mhd.exe`.

---

## Architecture

```
mhd.exe (single binary)
├── Tray thread
│   ├── System tray icon + context menu
│   ├── About dialog (native layered window)
│   └── Config editor (native dark popup)
├── Hook thread (WH_KEYBOARD_LL / WH_MOUSE_LL)
├── Worker thread (DDC/CI, PowerShell)
└── OSD thread (brightness overlay, native layered window)
```

All components live in one process with **0% CPU at idle** (blocking message
loops everywhere). The UI is pure Win32 — no `egui`, no `winit`, no OpenGL.

---

## Features

- **Key remapping** — replace any key or shortcut with another (e.g. `CapsLock` → `Alt+Shift`).
- **Mouse button bindings** — bind side buttons (XButton1/XButton2) to any action.
- **DDC/CI Brightness** — adjust monitor brightness with a native OSD overlay.
- **Run PowerShell** — execute arbitrary scripts on a hotkey.
- **Low‑level hooks** — `WH_KEYBOARD_LL` / `WH_MOUSE_LL`, sub‑millisecond response.
- **0 % CPU idle** — blocking message loops, zero polling in all threads.
- **Native themes** — load JSON colour themes from `%USERPROFILE%\.config\mhd\themes\`.
- **Styled settings panel** — native UI with theme selector, hover effects, and full DPI scaling.
- **Portable & tiny** — single binary, no installer, no runtime.

---

## Quick Start

### 1. Build
```powershell
cargo build --release
```
Requires Rust 1.85+.

### 2. Run
```powershell
.\mhd.exe              # tray + daemon (default)
.\mhd.exe --daemon     # headless, no tray
.\mhd.exe --quiet      # suppress startup messages
```

### 3. Configure
On first run, `mhd` creates a default config at:
```
%USERPROFILE%\.config\mhd\config.toml
```
Uncomment the bindings you want to enable, then restart mhd or
select **Reload Config** from the tray menu.

Use the **Edit Config** tray menu item to open the styled settings panel.

---

## Themes

mhd loads JSON colour themes from:

```
%USERPROFILE%\.config\mhd\themes
```

Set the active theme in `config.toml`:

```toml
theme = "glass_dark"
```

The file must be `%USERPROFILE%\.config\mhd\themes\glass_dark.json`.

### Supported colour keys

| Key | Used for |
|-----|----------|
| `background` | OSD / About / editor background |
| `surface` | Edit control background |
| `border` | Separator lines |
| `text` | Primary text |
| `text.muted` | Labels, version, hint, status |
| `element.active` | Accent / progress bar fill |
| `element.selected` | Selection highlight |
| `element.hover` | Hover state |

All keys are optional — missing values fall back to the built-in dark theme.

---

## Actions

| Action | Description |
|--------|-------------|
| `replace_key` | Suppress trigger, send different keys via `SendInput` |
| `set_brightness` | Adjust DDC/CI brightness (±5, or absolute value like 50) |
| `run_ps` | Run a PowerShell command |
| `quit` | Gracefully shut down mhd |

### Example bindings

```toml
# Quit (Ctrl+Alt+F12)
[[binding]]
trigger = "ctrl+alt+f12"
action = "quit"

# CapsLock → Alt+Shift (keyboard layout switch)
[[binding]]
trigger = "capslock"
action = "replace_key"
keys = "alt+shift"

# Brightness up / down
[[binding]]
trigger = "ctrl+alt+numpad_add"
action = "set_brightness"
value = "+5"

[[binding]]
trigger = "ctrl+alt+numpad_subtract"
action = "set_brightness"
value = "-5"
```

---

## Project Structure

```
mhd/
└── mhd-daemon/src/
    ├── main.rs           — CLI entry, startup orchestration
    ├── app.rs            — App lifecycle (run hooks, reload config)
    ├── hook.rs           — WH_KEYBOARD_LL / WH_MOUSE_LL hooks
    ├── tray.rs           — System tray icon + context menu
    ├── osd.rs            — Native Win32 layered OSD (brightness bar)
    ├── about.rs          — Styled native About dialog
    ├── config_editor.rs  — Styled native settings panel
    ├── native_theme.rs   — JSON theme loader + colour helpers
    ├── monitor.rs        — DDC/CI via dxva2.dll, EDID monitor name
    ├── trigger.rs        — Hotkey parsing
    ├── worker.rs         — Action execution thread
    ├── action.rs         — Action definitions and dispatch
    └── config.rs         — TOML config loading
```

---

## Internals

| Thread | Role |
|--------|------|
| **Tray** | System tray icon, context menu, About dialog, config editor |
| **Hook** | Low‑level keyboard/mouse hooks, blocking `GetMessageW` |
| **Worker** | DDC/CI calls and PowerShell execution (non‑blocking) |
| **OSD** | Layered overlay window, `MsgWaitForMultipleObjects`, auto‑hide |

All message loops use blocking wait APIs — **0 % CPU at idle**.

# mhd — Mouse & Hotkey Daemon for Windows

**mhd** is a lightweight background daemon that remaps keys, mouse buttons, and keyboard shortcuts using low-level Windows hooks. No drivers, no kernel components — just one portable `.exe` file.

Modernized with **Rust 2024** and a high-performance **egui** overlay system.

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
│   └───────────────▲───────────────────────────▲─────────────┘   │
│                   │                           │                 │
│         Direct internal calls           Direct internal calls   │
│                   │                           │                 │
│   ┌───────────────▼──────────────┐   ┌────────▼────────────────┐│
│   │         Tray Module          │   │      UI Overlays        ││
│   │                              │   │      (egui/eframe)      ││
│   │  🟢 Tray Icon                │   │                         ││
│   │  ┌────────────┐              │   │  🔅 Brightness Bar      ││
│   │  │Status: OK   │              │   │  ℹ️  About Window       ││
│   │  │Edit Config  │              │   │  🎨 Custom Themes      ││
│   │  │Reload Config│              │   └────────────────────────┘│
│   │  │Quit mhd     │              │                             │
│   │  └────────────┘              │                             │
│   └──────────────────────────────┘                             │
│                                                                 │
│   User runs:                                                    │
│   mhd.exe                                                       │
│   → starts daemon + tray + UI overlay                           │
└─────────────────────────────────────────────────────────────────┘
```

---

## Features

- **Key remapping** — replace any key or shortcut with another (e.g. `CapsLock` → `Alt+Shift`).
- **Mouse button bindings** — bind side buttons (XButton1/XButton2) to any action.
- **DDC/CI Brightness Overlay** — adjust monitor brightness with smooth visual feedback (OSD).
- **Modern UI** — hardware-accelerated overlays using `egui` 0.33.
- **Theme Support** — load Zed-compatible JSON themes (e.g., `One Dark`, `Nightfox`).
- **Run arbitrary PowerShell** — execute any script on a hotkey.
- **Low-level hooks** — highly responsive `WH_KEYBOARD_LL` / `WH_MOUSE_LL`.
- **Portable & Tiny** — single binary, Rust 2024, no installation required.

---

## Quick Start

### 1. Build from source
Requires Rust 1.85+ (for Edition 2024).

```powershell
cargo build --release
```

### 2. Configure
On first run, `mhd` creates a default config at:
`%USERPROFILE%\.config\mhd\config.toml`

To apply a theme, place a `.json` theme file (e.g. from Zed) in a `themes/` folder next to the exe or in the config dir, and set `theme = "one_dark"` in your `config.toml`.

---

## Actions

### `set_brightness`
Adjust monitor brightness via DDC/CI with a visual OSD.
- `value = "+5"` / `value = "-5"` / `value = "50"`

### `replace_key`
Suppress trigger and send a different key combination.
- `keys = "alt+shift"`, `keys = "ctrl+win+left"`

### `run_ps`
Run a PowerShell command: `command = "Start-Process wt"`

---

## Project Structure

```
mhd/
├── mhd-daemon/
│   ├── src/
│   │   ├── main.rs     # CLI & Entry
│   │   ├── app.rs      # App orchestration
│   │   ├── hook.rs     # Low-level Windows hooks
│   │   ├── ui.rs       # egui Overlay system
│   │   ├── tray.rs     # Win32 Tray Icon
│   │   ├── theme.rs    # Zed theme parser
│   │   ├── monitor.rs  # DDC/CI implementation
│   │   ├── worker.rs   # Async action processor
│   │   └── ...
└── themes/             # Included color schemes
```

---

## Internals

1. **Main Thread**: Orchestrates lifecycle and signals.
2. **Hook Thread**: Low-level message loop for keyboard/mouse events.
3. **UI Thread**: Dedicated `winit` event loop for hardware-accelerated overlays.
4. **Worker Thread**: Executes DDC/CI and shell commands to prevent hook lag.

# mhd — Mouse & Hotkey Daemon for Windows

**mhd** is a lightweight background daemon that remaps keys, mouse buttons,
and keyboard shortcuts using low-level Windows hooks. No drivers, no kernel
components — just one portable `mhd.exe`.

---

## Architecture

```text
mhd.exe (single binary)
├── Tray thread
│   ├── System tray icon + context menu
│   ├── About dialog (native layered window)
│   ├── Config editor (native themed window)
│   └── Volume Mixer launcher
├── Hook thread (WH_KEYBOARD_LL / WH_MOUSE_LL)
├── Worker thread (action dispatch: keys, PowerShell, DDC/CI, mixer)
├── OSD thread (brightness overlay, native layered window)
└── Volume Mixer thread (Core Audio + interactive layered window)
```

All components live in one process with **0% CPU at idle**. Threads use blocking
Win32 message/event waits (`GetMessageW`, `MsgWaitForMultipleObjects`, events),
not polling. The UI is pure Win32/GDI layered windows — no `egui`, no `winit`,
no OpenGL.

---

## Features

- **Key remapping** — replace any key or shortcut with another via `SendInput`.
- **Mouse button bindings** — bind side buttons (`mouseButton4`/`mouseButton5`) to actions.
- **DDC/CI brightness and VCP** — adjust monitor brightness or arbitrary VCP codes.
- **Brightness OSD** — native themed overlay for brightness changes.
- **Interactive Volume Mixer** — master volume + per-app audio sessions via Core Audio.
- **Run PowerShell** — execute arbitrary scripts/commands on a hotkey.
- **Low-level hooks** — `WH_KEYBOARD_LL` / `WH_MOUSE_LL` with lock-free hot path.
- **Config schemes** — switch between named binding schemes at runtime.
- **Native themes** — JSON colour themes from `%USERPROFILE%\.config\mhd\themes\`.
- **Styled settings panel** — native UI with theme selector, hover effects, DPI scaling.
- **System tray menu** — reload config, edit config, open volume mixer, about, quit.
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

```text
%USERPROFILE%\.config\mhd\config.toml
```

You can override the config path with:

```powershell
$env:MHD_CONFIG = "C:\path\to\config.toml"
```

Uncomment or add bindings, then restart mhd or select **Reload Config** from the
tray menu. Use **Edit Config** from the tray to open the native settings panel.

---

## Volume Mixer

Open it via the tray menu (**Volume Mixer**) or bind the action:

```toml
[[binding]]
trigger = "ctrl+alt+numpad_star"
action = "show_volume_mixer"
```

The mixer shows:

- `Master Volume` (default render endpoint)
- active per-application audio sessions

Controls:

- click a volume bar — set volume
- drag a volume bar — adjust continuously
- hover a row + mouse wheel — adjust by small steps
- drag the header — move the mixer window
- `Esc` — close

Auto-hide behaviour:

- on show: long timeout (`12s`)
- while mouse is over the window: timeout disabled
- after mouse leaves: short timeout (`2s`)

---

## Themes

mhd loads JSON colour themes from:

```text
%USERPROFILE%\.config\mhd\themes
```

Set the active theme in `config.toml`:

```toml
theme = "glass_dark"
```

The file must be:

```text
%USERPROFILE%\.config\mhd\themes\glass_dark.json
```

### Supported colour keys

| Key | Used for |
|-----|----------|
| `background` | OSD / About / editor / mixer background |
| `surface` | Edit control background |
| `border` | Separator lines |
| `text` | Primary text |
| `text.muted` | Labels, version, hints, status, secondary text |
| `element.active` | Accent / progress bar fill |
| `element.selected` | Selection highlight |
| `element.hover` | Hover state |

All keys are optional — missing values fall back to the built-in dark theme.

---

## Configuration

| Key | Default | Description |
|-----|---------|-------------|
| `active_scheme` | `"default"` | Startup binding scheme |
| `theme` | — | Active colour theme name (from `themes/` dir) |
| `volume_step` | `1` | Step size for `media_volume_up` / `media_volume_down` (each step sends one VK press) |

---

## Actions

| Action | Fields | Description |
|--------|--------|-------------|
| `replace_key` | `keys` | Suppress trigger and send different keys via `SendInput` |
| `set_brightness` | `value` | Adjust DDC/CI brightness (`+5`, `-5`, or absolute `50`) — *backward compat* |
| `brightness_up` | `value` | Increase monitor brightness (default `5`, configurable step) |
| `brightness_down` | `value` | Decrease monitor brightness (default `5`, configurable step) |
| `vcp` | `code`, `value` | Set or adjust arbitrary DDC/CI VCP code |
| `run_ps` | `command` | Run a PowerShell command |
| `switch_scheme` | `target_scheme` | Switch active binding scheme |
| `show_volume_mixer` | — | Show the interactive Volume Mixer overlay |
| `media_volume_up` | — | Increase system volume by one step |
| `media_volume_down` | — | Decrease system volume by one step |
| `media_mute` | — | Toggle system mute |
| `media_play_pause` | — | Play or pause current media |
| `media_stop` | — | Stop media playback |
| `media_last_track` | — | Go to previous track |
| `media_next_track` | — | Go to next track |
| `toggle_topmost` | — | Toggle always‑on‑top for the currently focused window |
| `quit` | — | Gracefully shut down mhd |

The config editor exposes actions grouped by category (General, Display, Media,
System) in a cascading menu. Advanced actions such as `vcp` and `switch_scheme`
can be edited directly in TOML.

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

# Brightness up / down (step defaults to 5, can be changed)
[[binding]]
trigger = "ctrl+alt+numpad_add"
action = "brightness_up"
value = "5"

[[binding]]
trigger = "ctrl+alt+numpad_subtract"
action = "brightness_down"
value = "5"

# Show Volume Mixer
[[binding]]
trigger = "ctrl+alt+numpad_star"
action = "show_volume_mixer"

# Open Windows Terminal
[[binding]]
trigger = "ctrl+alt+t"
action = "run_ps"
command = "Start-Process wt"

# Set monitor input to HDMI 1 (0x60 is Input Select, 17 is HDMI 1 on many monitors)
[[binding]]
trigger = "ctrl+alt+f1"
action = "vcp"
code = "0x60"
value = "17"
```

### Schemes

Bindings belong to the `default` scheme unless `scheme` is specified.

```toml
active_scheme = "default"

[[binding]]
scheme = "gaming"
trigger = "mouseButton4"
action = "replace_key"
keys = "ctrl"

[[binding]]
trigger = "ctrl+alt+g"
action = "switch_scheme"
target_scheme = "gaming"
```

---

## Project Structure

```text
mhd/
├── Cargo.toml                    — workspace
└── mhd-daemon/
    ├── Cargo.toml                — binary crate (`mhd`)
    └── src/
        ├── main.rs               — CLI entry, startup orchestration
        ├── app.rs                — App lifecycle and DaemonControl
        ├── platform.rs           — Win32 helper layer (keys/events)
        ├── action.rs             — Action definitions + action registry
        ├── worker.rs             — Action execution thread
        ├── hook.rs               — WH_KEYBOARD_LL / WH_MOUSE_LL hooks
        ├── trigger.rs            — Hotkey/mouse trigger parsing
        ├── tray.rs               — System tray icon + context menu
        ├── volume_mixer.rs       — Core Audio interactive mixer overlay
        ├── monitor.rs            — DDC/CI via dxva2.dll, EDID monitor name
        ├── native_theme.rs       — JSON theme loader + colour helpers
        ├── about.rs              — Styled native About dialog
        ├── config_editor.rs      — Styled native settings panel
        ├── config/
        │   ├── mod.rs            — Validated config model
        │   ├── raw.rs            — TOML-deserialised raw config
        │   └── path.rs           — Config path + example config
        └── osd/
            ├── mod.rs            — Brightness OSD thread/window
            └── painter.rs        — GDI/DIB drawing helpers
```

---

## Internals

| Thread | Role |
|--------|------|
| **Tray** | Tray icon, context menu, About dialog, config editor |
| **Hook** | Low-level keyboard/mouse hooks, blocking `GetMessageW`; lock-free state lookup in callbacks |
| **Worker** | Key send, scheme switch, DDC/CI calls, PowerShell, mixer show requests |
| **OSD** | Brightness layered overlay, `MsgWaitForMultipleObjects`, auto-hide |
| **Volume Mixer** | Core Audio enumeration, interactive layered overlay, hover/drag/wheel input |

The low-level hook hot path avoids mutex locking so desktop switches or UI stalls
do not block Windows low-level hook callbacks.

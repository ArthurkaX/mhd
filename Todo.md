# TODO: Replace legacy OSD UI with Native Win32 Layered OSD

## Objective

Replace the current heavyweight/legacy OSD implementation with a lightweight native Windows OSD:

- Brightness / volume OSD: **custom Win32 layered window**
- Rendering: initially **GDI/GDI+** or compatible lightweight Win32 drawing
- Runtime behavior: **0% CPU when idle**, no polling, no continuous repaint loop
- Architecture: no external UI process, no UDP bridge for OSD, no `eframe`/`egui` for OSD
- Visual style: modern, translucent, rounded, readable, minimal

The OSD should live inside the main `mhd.exe` process and be controlled through direct in-process messages/events.

---

## Non-goals

- Do not implement Windows Toast notifications for brightness/volume OSD.
- Do not keep `eframe`/`egui` for the OSD.
- Do not use a separate OSD process.
- Do not use polling timers for idle logic.
- Do not redesign the whole tray architecture in this task unless required by the OSD migration.

---

## Desired End State

```text
mhd.exe
 ├─ hotkey hook thread
 ├─ worker/action thread
 ├─ IPC thread
 ├─ native OSD thread/window
 └─ tray integration later
```

Brightness flow:

```text
hotkey pressed
  -> worker executes brightness action
  -> worker calls osd.show_brightness(value, monitor_name)
  -> OSD thread receives message
  -> native layered window appears/updates
  -> hide timer fires once after timeout
  -> window hides
```

Idle behavior:

```text
CPU: 0%
No repaint loop
No sleep polling
No UDP listener for OSD
```

---

# Delegation Plan

## Agent A — OSD Runtime / Thread / Public API

### Goal
Create a native OSD subsystem inside `mhd-daemon`.

### Files

- Add: `mhd-daemon/src/osd.rs`
- Modify: `mhd-daemon/src/main.rs`
- Modify: `mhd-daemon/src/worker.rs`
- Modify: `mhd-daemon/src/ui.rs` or remove/replace if it only exists for old OSD bridge

### Tasks

- [ ] Create `osd.rs` module.
- [ ] Define public handle:

```rust
pub struct OsdHandle { /* thread id / hwnd / channel */ }
```

- [ ] Expose API:

```rust
impl OsdHandle {
    pub fn show_brightness(&self, value: u8, monitor_name: String);
    pub fn show_volume(&self, value: u8); // optional placeholder
    pub fn shutdown(&self);
}
```

- [ ] Start a dedicated OSD UI thread from daemon startup.
- [ ] The OSD thread must own all OSD Win32 window state.
- [ ] Use `PostThreadMessageW` or a lightweight channel + message wakeup.
- [ ] The OSD thread must run a blocking `GetMessageW` loop.
- [ ] No `PeekMessageW` + sleep loops.
- [ ] No busy repaint loop.
- [ ] Add clean shutdown support.

### Acceptance Criteria

- [ ] `cargo check` passes.
- [ ] Daemon starts with an OSD thread.
- [ ] OSD thread id / handle is available to `worker.rs`.
- [ ] Idle CPU remains 0%.

---

## Agent B — Native Win32 Layered Window

### Goal
Implement the actual OSD window.

### Files

- `mhd-daemon/src/osd.rs`

### Window Requirements

Create a borderless topmost layered tool window:

- `WS_POPUP`
- `WS_EX_TOPMOST`
- `WS_EX_LAYERED`
- `WS_EX_TOOLWINDOW`
- optional: `WS_EX_TRANSPARENT` for click-through
- optional: `WS_EX_NOACTIVATE`

Behavior:

- [ ] Does not steal focus.
- [ ] Does not appear in Alt-Tab.
- [ ] Always appears above normal windows.
- [ ] Can be hidden/shown repeatedly.
- [ ] Positions itself on the active/current monitor, preferably the monitor associated with the brightness target.

### Rendering Requirements

Implement native drawing with one of:

- GDI+ preferred for rounded/antialiased UI
- GDI acceptable for first version
- Direct2D optional future upgrade, not required now

OSD visual style:

- [ ] Dark translucent rounded rectangle.
- [ ] Monitor name line, e.g. `Generic Monitor (EK251Q G)`.
- [ ] Label line, e.g. `Brightness`.
- [ ] Horizontal progress bar.
- [ ] Numeric percent value.
- [ ] Comfortable padding and font size.
- [ ] Looks good on high DPI displays.

Recommended layout:

```text
┌────────────────────────────────┐
│ Generic Monitor (EK251Q G)     │
│ Brightness                     │
│ ███████████░░░░░░░  65%         │
└────────────────────────────────┘
```

### Layered Rendering Options

Preferred:

- Render into a 32-bit ARGB memory bitmap.
- Use `UpdateLayeredWindow` for per-pixel alpha.

Alternative first-pass:

- Regular window + `WM_PAINT` + `SetLayeredWindowAttributes` alpha.

### Acceptance Criteria

- [ ] OSD appears visually correctly.
- [ ] OSD background is translucent.
- [ ] OSD text is readable.
- [ ] OSD does not steal focus.
- [ ] OSD does not appear in Alt-Tab.
- [ ] OSD can update while already visible.

---

## Agent C — Timer, Animation, and No-Polling Behavior

### Goal
Implement show/update/hide lifecycle without CPU waste.

### Files

- `mhd-daemon/src/osd.rs`

### Tasks

- [ ] Use Win32 timer or message-based one-shot timeout.
- [ ] When `show_brightness` is called:
  - update current OSD data
  - repaint immediately
  - show window
  - reset hide timer
- [ ] Hide window after configured timeout, e.g. 1000–1500 ms.
- [ ] Optional: fade-in/fade-out, but only if it does not require continuous idle repaint.
- [ ] If fade is implemented, use short timer-driven animation only while visible/fading.
- [ ] No animation loop when hidden.

### Recommended first version

Do **not** implement fade initially. Keep it simple:

```text
show/update -> visible
SetTimer(hide_timer, 1200ms)
WM_TIMER -> hide window
```

### Acceptance Criteria

- [ ] Repeated brightness key presses update the same OSD window.
- [ ] Hide timeout is reset on each update.
- [ ] Window hides after timeout.
- [ ] CPU returns to 0% after hiding.
- [ ] CPU does not stay at 5% while visible.

---

## Agent D — Remove Legacy OSD Stack

### Goal
Remove old heavyweight OSD technologies and communication path.

### Files

Likely affected:

- `mhd-daemon/src/ui.rs`
- `mhd-daemon/src/worker.rs`
- `mhd-daemon/src/main.rs`
- `mhd-ui/Cargo.toml`
- `mhd-ui/src/main.rs`
- root `Cargo.toml`
- `Cargo.lock`
- README docs

### Tasks

- [ ] Remove UDP OSD bridge if it is only used for `eframe` OSD.
- [ ] Remove calls that send brightness OSD data over UDP.
- [ ] Replace with direct `OsdHandle::show_brightness(...)` calls.
- [ ] Remove `eframe` dependency from the active daemon path.
- [ ] Remove `egui` dependency from the active daemon path.
- [ ] Remove `mhd-ui` from workspace if no longer used.
- [ ] Delete or archive obsolete `mhd-ui` source after confirming no tray code is needed from it.
- [ ] Remove build scripts/assets only used by old UI unless still needed by tray.
- [ ] Verify release binary no longer links OpenGL/winit/glutin stack.

### Acceptance Criteria

- [ ] `cargo tree` for active binary no longer includes `eframe`.
- [ ] `cargo tree` for active binary no longer includes `egui`.
- [ ] No UDP socket is used for brightness OSD.
- [ ] Only one active executable is required for OSD.
- [ ] Release binary builds cleanly.

---

## Agent E — Monitor Name Integration

### Goal
Ensure OSD displays the preferred monitor name.

### Current preference
The user accepts this format:

```text
Generic Monitor (EK251Q G)
```

### Tasks

- [ ] Preserve current monitor name resolution work.
- [ ] Prefer Windows display name if it includes useful model info.
- [ ] Avoid showing raw PnP fragments like `ACRODEF` unless no better value exists.
- [ ] If EDID parser returns only `EK251Q G`, optionally combine:

```text
Generic Monitor (EK251Q G)
```

- [ ] If Windows returns `Generic Monitor (EK251Q G)` directly, use it as-is.
- [ ] Cache resolved monitor names.

### Acceptance Criteria

- [ ] OSD should not show only `Generic Monitor` if model info is available.
- [ ] OSD should not show only `ACRODEF` if friendly name is available.
- [ ] `Generic Monitor (EK251Q G)` is acceptable and preferred over `ACRODEF`.

---

## Agent F — DPI and Multi-Monitor Placement

### Goal
Make the OSD appear correctly on modern Windows display setups.

### Tasks

- [ ] Make process/thread DPI aware if not already done.
- [ ] Calculate OSD size using DPI scaling.
- [ ] Position OSD on the target monitor when brightness change is monitor-specific.
- [ ] Fallback to foreground window monitor.
- [ ] Fallback to primary monitor.
- [ ] Keep OSD inside work area.

### Suggested priority

1. Primary monitor placement first.
2. Foreground window monitor fallback.
3. Target monitor mapping later.

### Acceptance Criteria

- [ ] OSD is not blurry on high DPI.
- [ ] OSD is centered or placed consistently.
- [ ] OSD does not appear off-screen.

---

## Agent G — Config and Theming

### Goal
Expose minimal OSD customization without overengineering.

### Files

- Config parser files
- README
- Example config

### Proposed config keys

```toml
[osd]
enabled = true
timeout_ms = 1200
position = "bottom-center" # center, bottom-center, top-center
opacity = 0.88
width = 420
height = 128
```

Optional later:

```toml
[osd.colors]
background = "#202020"
foreground = "#ffffff"
accent = "#4cc2ff"
bar_background = "#505050"
```

### Tasks

- [ ] Add OSD config struct.
- [ ] Provide defaults.
- [ ] Use defaults if config keys are missing.
- [ ] Do not block initial migration on full theming.

### Acceptance Criteria

- [ ] OSD works without config changes.
- [ ] Timeout can be configured.
- [ ] OSD can be disabled.

---

## Agent H — Testing and Validation

### Goal
Validate behavior, resource usage, and cleanup.

### Tests / Manual Checks

- [ ] `cargo check`
- [ ] `cargo build --release`
- [ ] Start daemon.
- [ ] Confirm idle CPU is 0%.
- [ ] Trigger brightness OSD once.
- [ ] Confirm CPU spike is short and returns to 0%.
- [ ] Hold brightness hotkey.
- [ ] Confirm OSD updates smoothly without multiple windows.
- [ ] Confirm RAM usage is lower than old `eframe` path.
- [ ] Confirm no `mhd-ui` process is required.
- [ ] Confirm OSD text displays monitor name.
- [ ] Confirm process exits cleanly.
- [ ] Confirm no locked `mhd.exe` remains after shutdown.

### Resource targets

Approximate goals, not hard guarantees:

```text
Idle CPU: 0%
Visible CPU: short spike only, not sustained 5%
RAM: significantly below old eframe-based UI
```

---

# Implementation Notes

## Win32 constants / APIs likely needed

Window:

- `RegisterClassW`
- `CreateWindowExW`
- `ShowWindow`
- `SetWindowPos`
- `DestroyWindow`
- `DefWindowProcW`
- `GetMessageW`
- `TranslateMessage`
- `DispatchMessageW`
- `PostThreadMessageW`
- `PostQuitMessage`

Layered window:

- `UpdateLayeredWindow`
- `SetLayeredWindowAttributes`
- `BLENDFUNCTION`
- `AC_SRC_ALPHA`

Painting:

- GDI: `CreateCompatibleDC`, `CreateDIBSection`, `SelectObject`, `DeleteObject`, `DeleteDC`
- GDI+: `GdiplusStartup`, `GdiplusShutdown`, graphics/path/brush/font APIs

Timers:

- `SetTimer`
- `KillTimer`
- `WM_TIMER`

Monitor placement:

- `MonitorFromWindow`
- `MonitorFromPoint`
- `GetMonitorInfoW`
- `GetForegroundWindow`

DPI:

- `SetProcessDpiAwarenessContext` or equivalent
- `GetDpiForWindow` / `GetDpiForMonitor` where available

---

# Suggested Milestones

## Milestone 1 — Minimal Native OSD

- Native OSD thread
- Plain topmost window
- Shows brightness text/progress
- Hides after timeout
- No `eframe` used for brightness OSD

## Milestone 2 — Layered Styled OSD

- Rounded translucent background
- Better text and progress bar
- High DPI sizing
- Stable monitor placement

## Milestone 3 — Cleanup

- Remove old UI crate/dependencies
- Remove UDP OSD path
- Update README/config examples
- Verify resource usage

---

# Final Acceptance Checklist

- [ ] Brightness OSD is rendered by native Win32 code.
- [ ] Old OSD technologies are removed from the active path.
- [ ] `eframe`/`egui` are no longer required for OSD.
- [ ] No external OSD process is required.
- [ ] No UDP OSD bridge remains.
- [ ] Idle CPU is 0%.
- [ ] CPU does not remain around 5% while OSD is visible.
- [ ] OSD looks modern enough: translucent, rounded, readable.
- [ ] OSD displays acceptable monitor name, e.g. `Generic Monitor (EK251Q G)`.
- [ ] Release build succeeds.

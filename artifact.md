# mhd — Minimal Hotkey Daemon Technical Specification

## 1. Purpose

`mhd` is a minimal Windows hotkey daemon written in Rust.

The primary goal is to replace `whkd` for a small set of global hotkeys while adding native mouse side-button support. The daemon must support both keyboard and mouse triggers, swallow matched input events, and execute configured actions.

Initial use cases:

- Replace `CapsLock` with `Alt+Shift` for keyboard layout switching.
- Use mouse side buttons for virtual desktop switching.
- Use keyboard combinations for virtual desktop switching and moving windows between desktops via PowerShell commands.

## 2. Background

Existing tool: `whkd` v0.2.10.

Investigation result:

- `whkd` does not currently support mouse buttons 4/5.
- `xbutton1`, `xbutton2`, `XBUTTON1`, `mouse4`, and similar names are rejected by the parser.
- The error is fatal:

```text
Invalid key name 'XBUTTON1' at src\main.rs:337
```

Root cause:

- `whkd` validates trigger tokens through `VKey::from_keyname()` from the `win-hotkeys` crate.
- The `VKey` enum does not expose parseable XButton1/XButton2 names.
- Upstream issue exists but mouse support is not implemented yet.

Decision:

- Do not wait for upstream `whkd` support.
- Build a dedicated minimal Rust daemon: `mhd`.

## 3. Name

Executable name:

```text
mhd.exe
```

Meaning:

```text
minimal hotkey daemon
```

## 4. Platform

Target platform:

```text
Windows 11
```

Implementation language:

```text
Rust
```

## 5. Non-goals

The first version should not implement:

- GUI configuration editor.
- Tray icon.
- Async runtime.
- Long-running shell session.
- Plugin system.
- whkd-compatible parser.
- Cross-platform support.
- Built-in virtual desktop COM API.

Virtual desktop actions will initially be performed through PowerShell commands configured by the user.

## 6. Resource Goals

The daemon should be minimal and idle efficiently.

Target characteristics:

| Metric | Target |
|---|---|
| Idle CPU | 0% |
| Working set RAM | As low as practical, ideally below 3 MB |
| Runtime model | Blocking Windows message loop; optional lightweight action dispatcher/worker |
| Async runtime | None |
| Shell session | None |

## 7. Architecture

### 7.1 Event Handling

Use native low-level Windows hooks:

- `WH_KEYBOARD_LL` for keyboard input.
- `WH_MOUSE_LL` for mouse input.

The process registers both hooks once at startup and then sleeps inside a blocking message loop:

```rust
let mut msg = MSG::default();
while GetMessageW(&mut msg, None, 0, 0).as_bool() {
    // The thread wakes only on input/message events.
}
```

Hook callbacks must be fast and non-blocking. They may only decode the event, check the in-memory trigger map, enqueue/post the action, and immediately return.

Long-running work is forbidden inside hook callbacks, including PowerShell spawn, child-process waiting, config parsing, file I/O, or heavy allocation.

### 7.1.1 Action Dispatch

Matched actions must be executed outside the low-level hook callback.

Required behavior:

1. Hook callback identifies the binding.
2. Hook callback enqueues/posts the action to a dispatcher.
3. Hook callback immediately returns `LRESULT(1)` to swallow the input.
4. A worker/message-loop handler executes the action in parallel with input handling.

Acceptable implementation options:

- `std::sync::mpsc` channel from hook callback to an action worker thread.
- `PostMessageW` to the daemon message loop / hidden window with an action id.

PowerShell commands and `SendInput` replacement actions must not run directly inside the hook callback.

### 7.1.2 Trigger Lookup

Config is read and validated once at startup. Runtime matching uses in-memory structures only.

The implementation should build trigger maps before installing hooks, for example:

```rust
HashMap<TriggerKey, BindingId>
```

Where `TriggerKey` contains:

- modifier bitset: `alt`, `ctrl`, `shift`, `win`;
- one non-modifier key/button virtual code.

The hook callback must avoid scanning raw config data or reparsing trigger strings on input events.

### 7.2 Input Swallowing

If an incoming input event matches a configured binding:

1. Enqueue/post the configured action for execution outside the hook callback.
2. Return `LRESULT(1)` from the hook procedure immediately.
3. Do not pass the event to the next hook/application.

This means all configured bindings replace the original input.

### 7.3 Mouse Side Buttons

Windows exposes side mouse buttons as `XBUTTON1` and `XBUTTON2` through `WM_XBUTTONDOWN` and `MSLLHOOKSTRUCT.mouseData`.

Public config names must be more user-friendly:

```text
mouseButton1
mouseButton2
```

Mapping:

| Config name | Windows meaning |
|---|---|
| `mouseButton1` | `XBUTTON1` |
| `mouseButton2` | `XBUTTON2` |

## 8. Configuration

### 8.1 Format

Config format:

```text
TOML
```

Reasoning:

- Native and familiar in the Rust ecosystem.
- Human-readable.
- Easy to parse with `serde` + `toml`.
- Easy to extend later.

### 8.2 Config Path

The only default config path is:

```text
%USERPROFILE%\.config\mhd\config.toml
```

No whkd config path compatibility is required.

### 8.3 First Run Behavior

If the config file does not exist:

1. Create directory:

```text
%USERPROFILE%\.config\mhd
```

2. Create file:

```text
%USERPROFILE%\.config\mhd\config.toml
```

3. Write a fully commented example config.
4. Print a message telling the user that the example config was created.
5. Print a message telling the user to uncomment bindings.
6. Exit without installing hooks.

### 8.4 Empty Config Behavior

If the config file exists but contains no active `[[binding]]` entries:

- Print an error/message:

```text
mhd: config empty: <path>
```

- Exit.

A config with only comments is considered empty.

### 8.5 Schemes

The config may define multiple named schemes. A scheme is a set of bindings. Only one scheme is active at a time.

Default scheme selection:

```toml
active_scheme = "default"
```

Rules:

- If `active_scheme` is omitted, the active scheme is `default`.
- If `scheme` is omitted on a binding, the binding belongs to `default`.
- All bindings in all schemes are parsed and validated at startup.
- Switching schemes is in-memory only; the config is not reread.
- A scheme switch changes which normal bindings are matched after the switch.
- Scheme-switching bindings may be global or scheme-specific.

Example:

```toml
[[binding]]
scheme = "default"
trigger = "alt+1"
action = "run_ps"
command = "Switch-Desktop -Desktop 0"

[[binding]]
scheme = "gaming"
trigger = "alt+1"
action = "replace_key"
keys = "f1"
```

### 8.6 Example Default Config

The initially generated config should be fully commented:

```toml
# mhd config
# Path: %USERPROFILE%\.config\mhd\config.toml
#
# Uncomment bindings to enable them.
#
# Optional startup scheme. If omitted, "default" is used.
# active_scheme = "default"

# Replace CapsLock with Alt+Shift for keyboard layout switching.
# [[binding]]
# trigger = "capslock"
# action = "replace_key"
# keys = "alt+shift"

# Switch to the left virtual desktop using mouse side button 1.
# [[binding]]
# trigger = "mouseButton1"
# action = "run_ps"
# command = "Switch-Desktop -Desktop (Get-LeftDesktop)"

# Switch to the right virtual desktop using mouse side button 2.
# [[binding]]
# trigger = "mouseButton2"
# action = "run_ps"
# command = "Switch-Desktop -Desktop (Get-RightDesktop)"

# Switch to desktop 1.
# [[binding]]
# trigger = "alt+1"
# action = "run_ps"
# command = "Switch-Desktop -Desktop 0"

# Move active window to desktop 1.
# [[binding]]
# trigger = "alt+shift+1"
# action = "run_ps"
# command = "Move-Window -Desktop 0"
```

## 9. Binding Model

Configuration contains an array of bindings:

```toml
[[binding]]
trigger = "capslock"
action = "replace_key"
keys = "alt+shift"

[[binding]]
trigger = "mouseButton1"
action = "run_ps"
command = "Switch-Desktop -Desktop (Get-LeftDesktop)"
```

### 9.1 Common Fields

| Field | Type | Required | Description |
|---|---|---:|---|
| `trigger` | string | yes | Hotkey or mouse trigger combination. |
| `action` | string | yes | Action type. |
| `scheme` | string | no | Binding scheme name. Defaults to `default`. |

### 9.2 Supported Actions

Initial supported action types:

| Action | Required field | Description |
|---|---|---|
| `replace_key` | `keys` | Swallow trigger and send replacement key combination. |
| `run_ps` | `command` | Swallow trigger and run PowerShell command. |
| `switch_scheme` | `target_scheme` | Switch active binding scheme in memory. |

Unknown action values are configuration errors.

## 10. Trigger Syntax

Triggers are `+`-separated combinations.

Supported modifiers:

```text
alt
ctrl
shift
win
```

Supported examples:

```text
capslock
alt+1
alt+shift+1
ctrl+alt+x
win+space
f1
f12
mouseButton1
mouseButton2
alt+mouseButton1
ctrl+shift+mouseButton2
```

Supported key names should cover ordinary QWERTY keyboard keys where practical:

- letters `a` through `z`;
- digits `0` through `9`;
- function keys `f1` through `f24`;
- modifiers `alt`, `ctrl`, `shift`, `win`;
- common control/navigation keys such as `capslock`, `space`, `tab`, `enter`, `esc`, `backspace`, `delete`, `insert`, `home`, `end`, `pageup`, `pagedown`, `left`, `right`, `up`, `down`;
- common punctuation/OEM keys for a QWERTY keyboard where Windows virtual-key mapping is stable;
- mouse side buttons `mouseButton1`, `mouseButton2`.

Letter triggers must be based on English QWERTY virtual/physical key identity and should not depend on the current keyboard layout where feasible. For example, `alt+a` means the `A` key, not the currently produced localized character.

Rules:

- Matching should be case-insensitive.
- Whitespace around tokens should be ignored.
- Unknown key names are configuration errors.
- Duplicate modifiers should be rejected as configuration errors.
- A trigger must contain exactly one non-modifier key/button.

## 11. Replace Key Action

Example:

```toml
[[binding]]
trigger = "capslock"
action = "replace_key"
keys = "alt+shift"
```

Behavior:

1. Detect `CapsLock` down event.
2. Swallow the original `CapsLock` event immediately in the hook callback.
3. Dispatch the synthetic `Alt+Shift` action for execution outside the hook callback.
4. Send synthetic `Alt+Shift` input using `SendInput`.

Key repeat behavior:

- If the user holds the trigger key and Windows produces repeated key-down events, `mhd` should repeat the configured replacement action for each repeated key-down event.
- Each repeated trigger event must be swallowed immediately.
- The trigger key-up event should also be swallowed so the original key does not leak to applications.

The `keys` value uses the same combination syntax as `trigger`.

Supported examples:

```text
alt+shift
ctrl+c
win+ctrl+right
```

## 12. PowerShell Action

Example:

```toml
[[binding]]
trigger = "mouseButton1"
action = "run_ps"
command = "Switch-Desktop -Desktop (Get-LeftDesktop)"
```

Execution command semantics:

```text
powershell -NoProfile -Command <command>
```

Implementation:

- Spawn a new PowerShell process per action, but never from inside the low-level hook callback.
- Do not keep a persistent shell session.
- Do not use async runtime.
- Use `std::process::Command` with separate arguments, not manual shell-string concatenation, for example:

```rust
Command::new("powershell")
    .args(["-NoProfile", "-Command", command])
    .spawn()?;
```

This avoids `cmd.exe` quoting issues and supports commands containing quotes better than building one command line string manually.

## 13. Switch Scheme Action

Example:

```toml
[[binding]]
trigger = "ctrl+alt+1"
action = "switch_scheme"
target_scheme = "default"

[[binding]]
trigger = "ctrl+alt+2"
action = "switch_scheme"
target_scheme = "gaming"
```

Behavior:

1. Swallow the trigger event.
2. Switch the active scheme in memory.
3. Log the new active scheme unless `--quiet` is enabled.

The target scheme must exist in the validated config. Switching to an unknown scheme is a startup configuration error.

## 14. CLI

Supported CLI forms:

```text
mhd
mhd --quiet
mhd --edit
```

### 14.1 Default Mode

```text
mhd
```

Behavior:

- Logging is enabled by default.
- Load config.
- Validate config.
- Install hooks.
- Enter message loop.

### 14.2 Quiet Mode

```text
mhd --quiet
```

Behavior:

- Disable normal console logging.
- Error output may still be used for fatal startup failures.
- Intended for background/autostart usage.

### 14.3 Edit Mode

```text
mhd --edit
```

Behavior:

1. If config does not exist, create the commented example config.
2. Open the config file in the default associated editor.
3. Exit.

On Windows this should be implemented through `ShellExecuteW`.

## 15. Logging

Logging is enabled by default.

Suggested startup logs:

```text
mhd: loaded config: C:\Users\<user>\.config\mhd\config.toml
mhd: loaded bindings: <n>
mhd: listening
```

Suggested action log:

```text
mhd: triggered: <trigger>
```

`--quiet` disables normal logs.

## 16. Error Handling

Startup errors should print a clear message and exit with non-zero status.

Startup order is strictly all-or-nothing:

```text
resolve config path
→ create example config if missing
→ read config
→ parse TOML
→ validate all bindings in all schemes
→ build in-memory trigger maps
→ install keyboard hook
→ install mouse hook
→ enter message loop
```

Hooks must be installed only after the full config has been parsed and validated successfully. Runtime input handling must use only the in-memory validated representation. The config file is not reread while the daemon is running.

Fatal errors:

- Config TOML parse error.
- Unknown key name.
- Unknown action type.
- Missing required field for action.
- Empty active config.
- Failed to install keyboard hook.
- Failed to install mouse hook.

Examples:

```text
mhd: config parse error: <details>
mhd: unknown key: xbutton1
mhd: unknown action: shell
mhd: config empty: C:\Users\<user>\.config\mhd\config.toml
mhd: failed to install mouse hook: <details>
```

## 17. Dependencies

Recommended Rust dependencies:

```toml
[dependencies]
serde = { version = "1", features = ["derive"] }
toml = "0.8"
windows = { version = "0.58", features = [
    "Win32_Foundation",
    "Win32_UI_WindowsAndMessaging",
    "Win32_UI_Input_KeyboardAndMouse",
    "Win32_System_Threading",
    "Win32_UI_Shell",
] }
```

Avoid unless clearly needed:

- `tokio`
- `clap`
- `anyhow`
- `color-eyre`
- parser combinator crates

Manual CLI parsing through `std::env::args()` is sufficient for the initial version.

## 18. Current Desired User Config

An active user config may look like this:

```toml
[[binding]]
trigger = "capslock"
action = "replace_key"
keys = "alt+shift"

[[binding]]
trigger = "mouseButton1"
action = "run_ps"
command = "Switch-Desktop -Desktop (Get-LeftDesktop)"

[[binding]]
trigger = "mouseButton2"
action = "run_ps"
command = "Switch-Desktop -Desktop (Get-RightDesktop)"

[[binding]]
trigger = "alt+1"
action = "run_ps"
command = "Switch-Desktop -Desktop 0"

[[binding]]
trigger = "alt+2"
action = "run_ps"
command = "Switch-Desktop -Desktop 1"

[[binding]]
trigger = "alt+shift+1"
action = "run_ps"
command = "Move-Window -Desktop 0"

[[binding]]
trigger = "alt+shift+2"
action = "run_ps"
command = "Move-Window -Desktop 1"
```

## 19. References

- whkd repo: `https://github.com/LGUG2Z/whkd`
- whkd mouse feature issue: `https://github.com/LGUG2Z/whkd/issues/32`
- VirtualDesktop PowerShell module: `Install-Module VirtualDesktop -Scope CurrentUser`
- Local whkd clone: `C:\Workspace\Libraries\side\whkd`
- Previous whkd config: `C:\Users\arthu\.config\whkdrc`

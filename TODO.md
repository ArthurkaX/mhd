# TODO.md — mhd project status

## Done ✅

- [x] Daemon (`mhd-daemon`) with all actions: `replace_key`, `run_ps`, `set_brightness`, `switch_scheme`, `quit`
- [x] Brightness module via DDC/CI (`dxva2.dll`), dynamic loading
- [x] IPC server over named pipe in daemon (`ipc.rs`)
- [x] Tray UI (`mhd-tray`): tray icon, context menu (Status / Edit Config / Reload Config / Restart Daemon / Quit mhd)
- [x] Launching daemon from UI via `CreateProcessW`
- [x] UI → daemon communication via named pipe (commands: `status`, `reload`, `shutdown`)
- [x] Icons: `mHD_16.png`, `mHD_32.png`, `mHD_256.png` in `icons/`
- [x] README.md with full documentation
- [x] Workspace Cargo: `mhd-daemon` + `mhd-ui`

## To Do 🔧

### New target plan: single `mhd.exe` instead of `mhd.exe` + `mhd-tray.exe`

Goal: one binary, idle CPU ≈ 0%, tray + daemon core by default, separate headless mode via `--daemon`.

#### Task Delegation

**Agent A — core/runtime** ✅
- [x] Extract common daemon launch from `main.rs` into `app.rs`
  - `App::new(config_path, quiet) -> App`
  - inside: config, worker, hooks
  - `AppHandle`: `status()`, `reload_config()`, `shutdown()`
- [x] Remove polling entirely: only blocking `GetMessageW`
- [ ] Verify idle CPU after launch without tray: should be ≈ 0%
- [x] Add CLI:
  - `mhd.exe` → daemon core + tray
  - `mhd.exe --daemon` / `--no-tray` → daemon core only
  - `mhd.exe --quiet` → fewer logs

**Agent B — tray module** ✅
- [x] Port code from `mhd-ui/src/main.rs` to `mhd-daemon/src/tray.rs`
- [x] Tray should work as an optional module inside a single process
  - hidden window
  - `Shell_NotifyIconW`
  - context menu
  - `NIM_DELETE` on shutdown
- [x] Tray commands should invoke `AppHandle` directly, without named pipe:
  - Status
  - Edit Config
  - Reload Config
  - Quit
- [x] Remove launching external `mhd.exe` via `CreateProcessW`

**Agent C — IPC/external control** ✅
- [x] Keep named pipe only for external control and headless mode
- [x] IPC commands should invoke `AppHandle`:
  - `status`
  - `reload`
  - `shutdown`
- [x] Fix `reload`: actually re-read config and rebuild trigger map
- [x] Verify IPC thread is not spinning in a busy loop and not consuming CPU (blocking `ReadFile`)

**Agent D — cleanup/build/docs** 🔧 (осталось)
- [ ] Update workspace: decide the fate of `mhd-ui`
  - either remove the crate
  - or temporarily leave it deprecated
- [ ] Update README for single exe
- [ ] Update TODO after migration
- [x] Verify `cargo check`
- [ ] Verify `cargo build --release`
- [ ] Verify launch:
  - `mhd.exe`
  - `mhd.exe --daemon`
  - `mhd.exe --daemon --quiet`

### Required

- [x] **Replace `static mut STATE` in `mhd-ui` with raw pointer**
  - edition 2021 accepts it with a warning, in 2024 it will be an error
  - Correct: `static STATE: SyncUnsafeCell<Option<State>>` or via `AtomicPtr`

- [x] **Remove `run_edit` from daemon** — function no longer used (config editing is now in UI)

- [x] **Add `let _ =` for all unused Results/BOOLs** — suppress warnings in both crates

- [x] **Check daemon process handle cleanup** — `PROCESS_INFORMATION` is not closed after `start_daemon()`

- [x] **Icon `mHD_32.png` next to exe** — `LoadImageW` looks for the icon relative to the exe path. Needs either embedding as a resource or copying next to `mhd-tray.exe` during installation

- [x] **Remove `VirtualDesktop` mention from example config** — it's not used

### Nice to Have

- [x] **Reload config without restarting daemon** — done via `AppHandle::reload_config()`, IPC `reload` actually re-reads config

- [ ] **Generic VCP command** (`action = "vcp"`) — contrast, input, volume, etc.

- [ ] **Icon in .exe** — embed via `embed-resource` + proper `.ico` (classic BMP, not PNG-compressed)

- [ ] **Daemon alive check** via timer in UI — currently the "running" status in the menu updates only when the menu is opened

- [ ] **Build .ico** — `icons/build_icon.ps1` script produces a PNG-compressed ICO incompatible with `rc.exe`. Need a classic BMP .ico

### Future

- [ ] Add `mouseWheel` as a trigger (mouse wheel)
- [ ] Multi-monitor support for `set_brightness`
- [ ] Installer / `winget` package
- [ ] Tests
- [ ] CI (GitHub Actions)

## Architecture (after single-exe migration, in progress)

```
mhd/
├── Cargo.toml              # workspace root (still has mhd-ui, to be cleaned)
├── README.md
├── TODO.md
├── icons/
│   ├── mHD_16.png
│   ├── mHD_32.png
│   ├── mHD_256.png
│   ├── mhd.rc / mhd.ico    # (temporarily broken)
│   ├── generate.ps1
│   └── build_icon.ps1
├── mhd-daemon/
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs          # entry point, CLI (--daemon, --quiet, --help)
│       ├── app.rs           # NEW: App + AppHandle, core orchestration
│       ├── action.rs        # Action enum, parsing
│       ├── brightness.rs    # DDC/CI via dxva2.dll
│       ├── config.rs        # TOML parsing, validation, schemes
│       ├── hook.rs          # WH_KEYBOARD_LL + WH_MOUSE_LL, message loop
│       ├── ipc.rs           # Named pipe server (external control only)
│       ├── tray.rs          # NEW: tray icon + context menu (in-process)
│       ├── trigger.rs       # Trigger/modifier parsing
│       └── worker.rs        # Action execution (SendInput/PowerShell/DDC)
└── mhd-ui/                  # ⚠️ DEPRECATED — код перенесён в mhd-daemon,
    ├── Cargo.toml           #    будет удалён после финальной проверки
    ├── mHD_32.png
    └── src/
        └── main.rs          # (больше не используется)
```

## IPC Protocol

Pipe: `\\.\pipe\mhd_ipc_pipe`

| Command (UI → daemon) | Daemon response | Action |
|---|---|---|
| `status` | `running\n` | Check if daemon is alive |
| `reload` | `reloaded\n` | Reload config (now actually works via `AppHandle::reload_config()`) |
| `shutdown` | `shutting_down\n` | Graceful shutdown via `running.store(false)` |

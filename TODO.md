# TODO.md — mhd project status

## Done ✅

- [x] Daemon (`mhd-daemon`) with all actions: `replace_key`, `run_ps`, `set_brightness`, `switch_scheme`, `quit`
- [x] Brightness module via DDC/CI (`dxva2.dll`), dynamic loading
- [x] IPC server over named pipe in daemon (`ipc.rs`)
- [x] Tray UI: tray icon, context menu (Status / Edit Config / Reload Config / Quit mhd)
- [x] Single binary migration: tray + daemon core in one process
- [x] CLI: `mhd.exe`, `mhd.exe --daemon`, `mhd.exe --quiet`
- [x] Icons: `mHD_16.png`, `mHD_32.png`, `mHD_256.png` in `icons/`
- [x] README.md updated for single exe
- [x] Workspace Cargo cleaned up: removed `mhd-ui`

## To Do 🔧

### Final Polish

- [ ] Update README for single exe (DONE in theory, but verifying)
- [ ] Update TODO after migration (DONE)
- [ ] Verify launch:
  - `mhd.exe`
  - `mhd.exe --daemon`
  - `mhd.exe --daemon --quiet`

### Required

- [x] **Replace `static mut STATE` in `mhd-ui` with raw pointer** (Done in `tray.rs`)
- [x] **Remove `run_edit` from daemon** (Done)
- [x] **Add `let _ =` for all unused Results/BOOLs** (Done)
- [x] **Check daemon process handle cleanup** (Not applicable in single-exe mode)
- [x] **Icon `mHD_32.png` next to exe** (Done, logic in `tray.rs` looks for it)
- [x] **Remove `VirtualDesktop` mention from example config** (Done)

### Nice to Have

- [x] **Reload config without restarting daemon** (Done via `AppHandle::reload_config()`)
- [ ] **Generic VCP command** (`action = "vcp"`) — contrast, input, volume, etc.
- [ ] **Icon in .exe** — embed via `embed-resource` + proper `.ico`
- [ ] **Build .ico** — need a classic BMP .ico

### Future

- [ ] Add `mouseWheel` as a trigger
- [ ] Multi-monitor support for `set_brightness`
- [ ] Installer / `winget` package
- [ ] Tests
- [ ] CI (GitHub Actions)

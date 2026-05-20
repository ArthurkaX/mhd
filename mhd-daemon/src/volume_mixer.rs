//! Interactive volume mixer overlay.
//!
//! Shows all active audio sessions with per-application volume sliders
//! in the same visual style as the brightness OSD.  The overlay is
//! interactive: click/drag on a volume bar to adjust, press Escape
//! to close.

use std::ffi::c_void;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::thread;

use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{
    COLORREF, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WAIT_EVENT, WAIT_OBJECT_0, WPARAM,
    CloseHandle,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DeleteDC, DeleteObject,
    DrawTextW, FillRect, GetDC, MonitorFromWindow, GetMonitorInfoW, ReleaseDC, SelectObject,
    SetBkMode, SetTextColor, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DEFAULT_QUALITY, DIB_RGB_COLORS, DT_END_ELLIPSIS, DT_LEFT, DT_RIGHT,
    DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, FW_NORMAL, HDC, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    OUT_DEFAULT_PRECIS, RGBQUAD, TRANSPARENT, AC_SRC_ALPHA, AC_SRC_OVER,
};
use windows::Win32::Media::Audio::{
    eMultimedia, eRender, IAudioSessionControl, IAudioSessionControl2,
    IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, ISimpleAudioVolume,
};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CreateEventW, SetEvent, QueryFullProcessImageNameW, INFINITE, OpenProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_NAME_WIN32,
};
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetDesktopWindow, KillTimer,
    MsgWaitForMultipleObjects, PeekMessageW, RegisterClassW, SetTimer, ShowWindow,
    TranslateMessage, UpdateLayeredWindow, CS_HREDRAW, CS_VREDRAW, PM_REMOVE, QS_ALLINPUT,
    SW_HIDE, SW_SHOWNA, SWP_NOMOVE, SWP_NOZORDER, SetWindowPos, ULW_ALPHA, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_QUIT, WM_TIMER, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WNDCLASSW, MSG,
};
use windows::core::Interface;

use crate::native_theme::NativeTheme;

// ── Constants ──────────────────────────────────────────────────────────

#[allow(non_upper_case_globals)]
const CLSID_MMDeviceEnumerator: GUID = GUID::from_u128(0xBCDE0395_E52F_467C_8E3D_C4579291692E);

const MIXER_WIDTH_BASE: i32 = 440;
const MIXER_MIN_HEIGHT_BASE: i32 = 120;
const ROW_HEIGHT_BASE: i32 = 40;
const HEADER_HEIGHT_BASE: i32 = 44;
const PAD_BASE: i32 = 16;
const BAR_HEIGHT_BASE: i32 = 8;
const HIDE_TIMEOUT_MS: u32 = 12000;
const HIDE_TIMER_ID: usize = 2;
const RADIUS_BASE: i32 = 14;

// ── Global handle ──────────────────────────────────────────────────────

static MIXER_HANDLE: OnceLock<MixerHandle> = OnceLock::new();
static MIXER_THEME: LazyLock<Mutex<NativeTheme>> =
    LazyLock::new(|| Mutex::new(NativeTheme::default()));

#[allow(dead_code)]
pub fn set_theme(theme: NativeTheme) {
    *MIXER_THEME.lock().unwrap() = theme;
}

/// Show the volume mixer overlay (non-blocking).
pub fn show() {
    let handle = MIXER_HANDLE.get_or_init(|| {
        let (handle, thread) = start_mixer_thread();
        drop(thread); // detach
        handle
    });
    let _ = handle.signal();
}

// ── Handle ─────────────────────────────────────────────────────────────

#[derive(Clone)]
struct MixerHandle {
    event: HANDLE,
}
unsafe impl Send for MixerHandle {}
unsafe impl Sync for MixerHandle {}

impl MixerHandle {
    fn signal(&self) -> Result<(), ()> {
        unsafe { SetEvent(self.event) }.map_err(|_| ())
    }
}

// ── Session data ───────────────────────────────────────────────────────

#[derive(Clone)]
#[allow(dead_code)]
struct SessionInfo {
    name: String,
    volume: f32,
    muted: bool,
    pid: u32,
}

// ── Mixer thread state ─────────────────────────────────────────────────

struct MixerState {
    sessions: Vec<SessionInfo>,
    volume_controls: Vec<Option<ISimpleAudioVolume>>,
    endpoint_volume: Option<IAudioEndpointVolume>,
    theme: NativeTheme,
}

// ── Thread entry point ─────────────────────────────────────────────────

fn start_mixer_thread() -> (MixerHandle, thread::JoinHandle<()>) {
    let event = unsafe { CreateEventW(None, false, false, None).expect("CreateEventW for mixer") };
    let handle = MixerHandle { event };

    let handle_clone = handle.clone();
    let join = thread::Builder::new()
        .name("mhd-mixer".into())
        .spawn(move || {
            mixer_thread(handle_clone);
        })
        .expect("spawn mixer thread");

    (handle, join)
}

fn mixer_thread(handle: MixerHandle) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let cls_name = crate::osd::to_utf16_z("mhd_mixer_cls");
    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let hinstance: windows::Win32::Foundation::HINSTANCE = hinst.into();

    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(mixer_wndproc),
        hInstance: hinstance,
        lpszClassName: PCWSTR::from_raw(cls_name.as_ptr()),
        ..Default::default()
    };
    unsafe {
        let _ = RegisterClassW(&wc);
    }

    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(cls_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            MIXER_WIDTH_BASE,
            MIXER_MIN_HEIGHT_BASE,
            None,
            None,
            hinstance,
            None,
        )
    } {
        Ok(h) => h,
        Err(_) => return,
    };

    let dpi = unsafe { GetDpiForWindow(hwnd) } as f32;
    let scale = dpi / 96.0;

    let mixer_w = (MIXER_WIDTH_BASE as f32 * scale) as i32;

    let mut state = MixerState {
        sessions: Vec::new(),
        volume_controls: Vec::new(),
        endpoint_volume: None,
        theme: NativeTheme::default(),
    };

    let work = monitor_work_rect();
    let mut dragging_row: Option<usize> = None;

    loop {
        let wait_handles = [handle.event];
        let res = unsafe {
            MsgWaitForMultipleObjects(Some(&wait_handles), false, INFINITE, QS_ALLINPUT)
        };

        // WAIT_OBJECT_0 + nhandles = messages available (same constant
        // as OSD's MSG_ARRIVED = WAIT_EVENT(1) for 1 handle).
        const MSG_ARRIVED: WAIT_EVENT = WAIT_EVENT(1);

        match res {
            WAIT_OBJECT_0 => {
                refresh_sessions(&mut state);
                state.theme = MIXER_THEME.lock().unwrap().clone();
                paint_mixer(hwnd, &state, &work, mixer_w, scale);
                unsafe {
                    let _ = ShowWindow(hwnd, SW_SHOWNA);
                    let _ = SetTimer(hwnd, HIDE_TIMER_ID, HIDE_TIMEOUT_MS, None);
                }
            }
            MSG_ARRIVED => {
                let mut msg = MSG::default();
                unsafe {
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        if msg.message == WM_QUIT {
                            break;
                        }
                        if msg.message == WM_KEYDOWN && msg.wParam.0 as u32 == 0x1B {
                            let _ = ShowWindow(hwnd, SW_HIDE);
                            let _ = KillTimer(hwnd, HIDE_TIMER_ID);
                        }
                        if msg.message == WM_TIMER && msg.wParam.0 == HIDE_TIMER_ID {
                            let _ = ShowWindow(hwnd, SW_HIDE);
                            let _ = KillTimer(hwnd, HIDE_TIMER_ID);
                        }

                        if msg.hwnd == hwnd {
                            match msg.message {
                                WM_LBUTTONDOWN => {
                                    let (x, y) = point_from_lparam(msg.lParam);
                                    if let Some((row, volume)) = hit_test_volume_bar(
                                        &state,
                                        x,
                                        y,
                                        mixer_w,
                                        scale,
                                    ) {
                                        dragging_row = Some(row);
                                        let _ = SetCapture(hwnd);
                                        set_row_volume(&mut state, row, volume);
                                        paint_mixer(hwnd, &state, &work, mixer_w, scale);
                                        let _ = SetTimer(hwnd, HIDE_TIMER_ID, HIDE_TIMEOUT_MS, None);
                                        continue;
                                    }
                                }
                                WM_MOUSEMOVE => {
                                    if let Some(row) = dragging_row {
                                        let (x, _) = point_from_lparam(msg.lParam);
                                        let volume = volume_from_x(x, mixer_w, scale);
                                        set_row_volume(&mut state, row, volume);
                                        paint_mixer(hwnd, &state, &work, mixer_w, scale);
                                        let _ = SetTimer(hwnd, HIDE_TIMER_ID, HIDE_TIMEOUT_MS, None);
                                        continue;
                                    }
                                }
                                WM_LBUTTONUP => {
                                    if dragging_row.take().is_some() {
                                        let _ = ReleaseCapture();
                                        continue;
                                    }
                                }
                                _ => {}
                            }
                        }

                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            }
            _ => {
                // Unknown return — continue loop, don't crash the thread
                continue;
            }
        }
    }

    // Unreachable in practice — the thread lives until process exit.
    // Cleanup is handled by the OS. Keep this for correctness.
    #[allow(unreachable_code)]
    unsafe {
        let _ = DestroyWindow(hwnd);
        CoUninitialize();
    }
}

// ── Audio session enumeration ──────────────────────────────────────────

fn refresh_sessions(state: &mut MixerState) {
    state.sessions.clear();
    state.volume_controls.clear();

    let device = match get_default_render_device() {
        Ok(d) => d,
        Err(_) => return,
    };

    state.endpoint_volume = get_endpoint_volume(&device);

    let master_volume = state
        .endpoint_volume
        .as_ref()
        .and_then(|ev| get_master_volume_level(ev).ok())
        .unwrap_or(0.5);
    let master_muted = state
        .endpoint_volume
        .as_ref()
        .and_then(|ev| get_endpoint_mute(ev).ok())
        .unwrap_or(false);

    state.sessions.push(SessionInfo {
        name: "Master Volume".into(),
        volume: master_volume,
        muted: master_muted,
        pid: 0,
    });
    state.volume_controls.push(None);

    // Enumerate per-app sessions
    if let Ok(manager) = get_session_manager(&device) {
        if let Ok(enumerator) = unsafe { manager.GetSessionEnumerator() } {
            let count = unsafe { enumerator.GetCount().unwrap_or(0) };
            for i in 0..count {
                if let Ok(control) = unsafe { enumerator.GetSession(i) } {
                    if let Ok(sd) = add_session_from_control(&control) {
                        state.sessions.push(sd.info);
                        state.volume_controls.push(sd.control);
                    }
                }
            }
        }
    }

    // Sort (Master first, then alphabetically)
    if state.sessions.len() > 1 {
        let mut zipped: Vec<(SessionInfo, Option<ISimpleAudioVolume>)> =
            state.sessions.drain(1..).zip(state.volume_controls.drain(1..)).collect();
        zipped.sort_by(|a, b| a.0.name.to_lowercase().cmp(&b.0.name.to_lowercase()));
        for (info, ctrl) in zipped {
            state.sessions.push(info);
            state.volume_controls.push(ctrl);
        }
    }
}

struct SessionData {
    info: SessionInfo,
    control: Option<ISimpleAudioVolume>,
}

fn add_session_from_control(control: &IAudioSessionControl) -> Result<SessionData, ()> {
    unsafe {
        let control2: IAudioSessionControl2 = control.cast().map_err(|_| ())?;
        let pid = control2.GetProcessId().map_err(|_| ())?;

        if pid == 0 {
            return Err(());
        }

        let name = get_session_display_name(&control2, pid);
        let volume_control: ISimpleAudioVolume = control.cast().map_err(|_| ())?;
        let volume = volume_control.GetMasterVolume().map_err(|_| ())?;
        let muted = volume_control.GetMute().map_err(|_| ())?;

        Ok(SessionData {
            info: SessionInfo { name, volume, muted: muted.into(), pid },
            control: Some(volume_control),
        })
    }
}

fn get_master_volume_level(ev: &IAudioEndpointVolume) -> Result<f32, ()> {
    unsafe { ev.GetMasterVolumeLevelScalar().map_err(|_| ()) }
}

fn get_endpoint_mute(ev: &IAudioEndpointVolume) -> Result<bool, ()> {
    unsafe { ev.GetMute().map(|b| b.as_bool()).map_err(|_| ()) }
}

fn get_default_render_device() -> Result<IMMDevice, ()> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&CLSID_MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|_| ())?;
        enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia).map_err(|_| ())
    }
}

fn get_endpoint_volume(device: &IMMDevice) -> Option<IAudioEndpointVolume> {
    unsafe { device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok() }
}

fn get_session_manager(device: &IMMDevice) -> Result<IAudioSessionManager2, ()> {
    unsafe { device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None).map_err(|_| ()) }
}

fn get_session_display_name(control2: &IAudioSessionControl2, pid: u32) -> String {
    unsafe {
        if let Ok(name) = control2.GetDisplayName() {
            let s = name.to_string().unwrap_or_default();
            if !s.is_empty() {
                return s;
            }
        }
    }
    get_process_name(pid).unwrap_or_else(|| format!("PID {}", pid))
}

fn get_process_name(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        if ok.is_ok() && len > 0 {
            let name = String::from_utf16_lossy(&buf[..len as usize]);
            let path = std::path::Path::new(&name);
            return path.file_stem().map(|s| s.to_string_lossy().into_owned());
        }
    }
    None
}

// ── Monitor work rect ──────────────────────────────────────────────────

fn monitor_work_rect() -> RECT {
    unsafe {
        let desktop = GetDesktopWindow();
        let hmon = MonitorFromWindow(desktop, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(hmon, &mut info);
        info.rcWork
    }
}

// ── Painting ───────────────────────────────────────────────────────────

fn paint_mixer(
    hwnd: HWND,
    state: &MixerState,
    work: &RECT,
    width: i32,
    scale: f32,
) {
    let screen_dc = unsafe { GetDC(None) };

    let row_count = state.sessions.len() as i32;
    let pad = (PAD_BASE as f32 * scale) as i32;
    let font_h = -(14.0 * scale) as i32;
    let font_small_h = -(12.0 * scale) as i32;
    let row_h = (ROW_HEIGHT_BASE as f32 * scale) as i32;
    let header_h = (HEADER_HEIGHT_BASE as f32 * scale) as i32;

    let content_h = pad + header_h + row_count * row_h + pad;
    let total_h = content_h.max((MIXER_MIN_HEIGHT_BASE as f32 * scale) as i32);

    unsafe {
        let _ = SetWindowPos(hwnd, None, 0, 0, width, total_h, SWP_NOMOVE | SWP_NOZORDER);
    }

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -total_h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: [RGBQUAD::default(); 1],
    };

    let mut bits: *mut c_void = std::ptr::null_mut();
    let dib = unsafe { CreateDIBSection(screen_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) };
    let Ok(dib) = dib else {
        unsafe { let _ = ReleaseDC(None, screen_dc); }
        return;
    };

    let dib_dc = unsafe { CreateCompatibleDC(screen_dc) };
    let old_bmp = unsafe { SelectObject(dib_dc, dib) };

    let theme = &state.theme;
    let radius = (RADIUS_BASE as f32 * scale) as i32;

    unsafe {
        let pixels = std::slice::from_raw_parts_mut(
            bits as *mut u32,
            (width * total_h) as usize,
        );
        crate::osd::painter::draw_rounded_rect(pixels, width, total_h, radius, theme.background);
    }

    let font_name = crate::osd::to_utf16_z("Segoe UI");

    let hfont = unsafe {
        CreateFontW(
            font_h, 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0,
            DEFAULT_CHARSET.0 as u32, OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32, DEFAULT_QUALITY.0 as u32,
            FF_DONTCARE.0 as u32, PCWSTR::from_raw(font_name.as_ptr()),
        )
    };
    let hfont_small = unsafe {
        CreateFontW(
            font_small_h, 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0,
            DEFAULT_CHARSET.0 as u32, OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32, DEFAULT_QUALITY.0 as u32,
            FF_DONTCARE.0 as u32, PCWSTR::from_raw(font_name.as_ptr()),
        )
    };

    let old_font = unsafe { SelectObject(dib_dc, hfont) };
    unsafe {
        let _ = SetBkMode(dib_dc, TRANSPARENT);
        let _ = SetTextColor(dib_dc, theme.text.to_colorref());
    }

    // ── Header ──
    let header_y = pad;
    let mut header_rc = RECT {
        left: pad,
        top: header_y,
        right: width - pad,
        bottom: header_y + font_h.abs() + 4,
    };
    let mut header_wz = crate::osd::to_utf16_z("Volume Mixer");
    unsafe {
        let _ = DrawTextW(
            dib_dc, &mut header_wz, &mut header_rc,
            DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
    }

    // Count
    let count_str = format!("{} sessions", state.sessions.len());
    unsafe {
        let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref());
    }
    let mut count_wz = crate::osd::to_utf16_z(&count_str);
    unsafe {
        let _ = DrawTextW(
            dib_dc, &mut count_wz, &mut header_rc,
            DT_RIGHT | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
    }

    // Separator
    let sep_y = header_y + font_h.abs() + 8;
    {
        let sep_brush = unsafe { CreateSolidBrush(theme.border.to_colorref()) };
        let sep_rc = RECT {
            left: pad, top: sep_y, right: width - pad, bottom: sep_y + 1,
        };
        unsafe {
            let _ = FillRect(dib_dc, &sep_rc, sep_brush);
            let _ = DeleteObject(sep_brush);
        }
    }

    // ── Rows ──
    unsafe {
        let _ = SelectObject(dib_dc, hfont_small);
    }

    let bar_h = (BAR_HEIGHT_BASE as f32 * scale).max(3.0) as i32;
    let label_w = (120.0 * scale) as i32;
    let bar_x = pad + label_w + pad;
    let bar_max_w = width - bar_x - pad - 50 - pad;

    for (i, session) in state.sessions.iter().enumerate() {
        let row_y = sep_y + 8 + (i as i32) * row_h;
        let mid_y = row_y + row_h / 2;

        // App name
        unsafe {
            let _ = SetTextColor(dib_dc, theme.text.to_colorref());
        }
        let mut name_rc = RECT {
            left: pad,
            top: row_y,
            right: pad + label_w,
            bottom: row_y + row_h,
        };
        let mut name_wz = crate::osd::to_utf16_z(&session.name);
        unsafe {
            let _ = DrawTextW(
                dib_dc, &mut name_wz, &mut name_rc,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            );
        }

        // Volume bar track
        let bar_y = mid_y - bar_h / 2;
        let track_color = if session.muted { theme.text_muted } else { theme.bar_background };
        let track_brush = unsafe { CreateSolidBrush(track_color.to_colorref()) };
        let track_rc = RECT {
            left: bar_x, top: bar_y, right: bar_x + bar_max_w, bottom: bar_y + bar_h,
        };
        unsafe {
            let _ = FillRect(dib_dc, &track_rc, track_brush);
            let _ = DeleteObject(track_brush);
        }

        // Volume bar fill
        let fill_w = (bar_max_w as f32 * session.volume).max(1.0) as i32;
        let fill_color = if session.muted { theme.text_muted } else { theme.accent };
        let fill_brush = unsafe { CreateSolidBrush(fill_color.to_colorref()) };
        let fill_rc = RECT {
            left: bar_x, top: bar_y, right: bar_x + fill_w, bottom: bar_y + bar_h,
        };
        unsafe {
            let _ = FillRect(dib_dc, &fill_rc, fill_brush);
            let _ = DeleteObject(fill_brush);
        }

        // Percentage
        let pct = format!("{}%", (session.volume * 100.0) as u32);
        let pct_x = bar_x + bar_max_w + 8;
        let mut pct_rc = RECT {
            left: pct_x, top: row_y, right: pct_x + 44, bottom: row_y + row_h,
        };
        unsafe {
            let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref());
        }
        let mut pct_wz = crate::osd::to_utf16_z(&pct);
        unsafe {
            let _ = DrawTextW(
                dib_dc, &mut pct_wz, &mut pct_rc,
                DT_RIGHT | DT_SINGLELINE | DT_VCENTER,
            );
        }
    }

    unsafe {
        let _ = SelectObject(dib_dc, old_font);
        let _ = DeleteObject(hfont);
        let _ = DeleteObject(hfont_small);
    }

    crate::osd::painter::fix_gdi_alpha(bits, width, total_h, theme.background);

    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };

    let pt_src = POINT { x: 0, y: 0 };
    let sz = SIZE { cx: width, cy: total_h };
    let pos_x = work.left + (work.right - work.left - width) / 2;
    let pos_y = work.top + (work.bottom - work.top - total_h) / 2;
    let pt_dst = POINT { x: pos_x, y: pos_y };

    unsafe {
        let _ = UpdateLayeredWindow(
            hwnd, HDC::default(),
            Some(&pt_dst), Some(&sz),
            dib_dc, Some(&pt_src),
            COLORREF(0), Some(&blend),
            ULW_ALPHA,
        );
    }

    unsafe {
        let _ = SelectObject(dib_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(dib_dc);
        let _ = ReleaseDC(None, screen_dc);
    }
}

// ── Interaction ────────────────────────────────────────────────────────

fn point_from_lparam(lparam: LPARAM) -> (i32, i32) {
    let x = (lparam.0 & 0xffff) as i16 as i32;
    let y = ((lparam.0 >> 16) & 0xffff) as i16 as i32;
    (x, y)
}

fn hit_test_volume_bar(
    state: &MixerState,
    x: i32,
    y: i32,
    width: i32,
    scale: f32,
) -> Option<(usize, f32)> {
    let pad = (PAD_BASE as f32 * scale) as i32;
    let font_h_abs = (14.0 * scale) as i32;
    let row_h = (ROW_HEIGHT_BASE as f32 * scale) as i32;
    let sep_y = pad + font_h_abs + 8;

    let label_w = (120.0 * scale) as i32;
    let bar_x = pad + label_w + pad;
    let bar_max_w = width - bar_x - pad - 50 - pad;

    if x < bar_x || x > bar_x + bar_max_w {
        return None;
    }

    for i in 0..state.sessions.len() {
        let row_y = sep_y + 8 + (i as i32) * row_h;
        if y >= row_y && y < row_y + row_h {
            return Some((i, volume_from_x(x, width, scale)));
        }
    }

    None
}

fn volume_from_x(x: i32, width: i32, scale: f32) -> f32 {
    let pad = (PAD_BASE as f32 * scale) as i32;
    let label_w = (120.0 * scale) as i32;
    let bar_x = pad + label_w + pad;
    let bar_max_w = width - bar_x - pad - 50 - pad;
    ((x - bar_x) as f32 / bar_max_w as f32).clamp(0.0, 1.0)
}

fn set_row_volume(state: &mut MixerState, row: usize, volume: f32) {
    let volume = volume.clamp(0.0, 1.0);

    if row == 0 {
        if let Some(endpoint) = state.endpoint_volume.as_ref() {
            unsafe {
                let _ = endpoint.SetMasterVolumeLevelScalar(volume, std::ptr::null());
                let _ = endpoint.SetMute(false, std::ptr::null());
            }
        }
    } else if let Some(Some(control)) = state.volume_controls.get(row) {
        unsafe {
            let _ = control.SetMasterVolume(volume, std::ptr::null());
            let _ = control.SetMute(false, std::ptr::null());
        }
    }

    if let Some(session) = state.sessions.get_mut(row) {
        session.volume = volume;
        session.muted = false;
    }
}

// ── Window procedure ───────────────────────────────────────────────────

extern "system" fn mixer_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

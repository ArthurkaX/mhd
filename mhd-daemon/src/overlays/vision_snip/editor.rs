//! Annotation description editor for Vision Snip.
//!
//! A small popup window with a RichEdit control for editing the
//! description text of a single annotation.  Has Cancel and Apply
//! buttons.  The DescriptionEditor owns the window state, which
//! outlives window destruction so callers can query `was_applied()`
//! and `get_text()` even after the window closes.

use std::ptr::NonNull;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_ESCAPE, VK_RETURN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use crate::core::native_theme::{Argb, NativeTheme};
use crate::osd::to_utf16_z;
use crate::win32::text_host::{TextHost, TextHostKind};

// ── Constants ──────────────────────────────────────────────────────────

const W: i32 = 400;
const H: i32 = 140;
const PAD: i32 = 10;
const HEADER_H: i32 = 30;
const BTN_H: i32 = 26;
const BTN_W: i32 = 70;
const BTN_GAP: i32 = 10;
const CLS: &str = "mhd_vs_editor_cls";

const WM_APP_APPLY: u32 = WM_APP;
const WM_APP_CANCEL: u32 = WM_APP + 1;
/// Posted to the parent window after the editor closes.
const WM_APP_EDITOR_DONE: u32 = WM_APP + 10;

// ── Window state ───────────────────────────────────────────────────────

struct EditorState {
    text_host: TextHost,
    edit_font: HFONT,
    theme: NativeTheme,
    label: char,
    applied: bool,
}

// ── DescriptionEditor ──────────────────────────────────────────────────

/// A small popup window for editing an annotation's description.
///
/// The editor owns its state until dropped.  Callers can query the
/// result after the window has been closed.
pub struct DescriptionEditor {
    hwnd: HWND,
    state: NonNull<EditorState>, // Box we own; freed in Drop
}

// SAFETY: EditorState is owned by this struct; the raw pointer is only
// used to access fields that are pinned for the struct's lifetime.
unsafe impl Send for DescriptionEditor {}
unsafe impl Sync for DescriptionEditor {}

impl DescriptionEditor {
    /// Create the editor popup.
    ///
    /// Positioned centered on `parent`.
    pub fn create(parent: HWND, theme: &NativeTheme, label: char, initial: &str) -> Option<Self> {
        let cls = to_utf16_z(CLS);
        let hi: HINSTANCE = unsafe { GetModuleHandleW(None).unwrap_or_default() }.into();

        unsafe {
            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                hInstance: hi,
                hbrBackground: HBRUSH(2 as _),
                lpszClassName: PCWSTR::from_raw(cls.as_ptr()),
                ..Default::default()
            };
            let _ = RegisterClassW(&wc);
        }

        let ex_style = if theme.background.a < 255 {
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED
        } else {
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW
        };

        let hwnd = unsafe {
            CreateWindowExW(
                ex_style,
                PCWSTR::from_raw(cls.as_ptr()),
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                W,
                H,
                parent, // HWND parent directly
                None,
                hi,
                None,
            )
        }
        .ok()?;

        if theme.background.a < 255 {
            unsafe {
                let _ =
                    SetLayeredWindowAttributes(hwnd, COLORREF(0), theme.background.a, LWA_ALPHA);
            }
        }

        // ── Create the RichEdit control ──────────────────────────────
        let brush_color = if theme.surface.a == 255 {
            theme.surface
        } else {
            theme.surface.blend_over(theme.background)
        };

        let edit_h = H - HEADER_H - BTN_H - PAD * 3;

        let text_host = TextHost::create(
            TextHostKind::RichEdit,
            hwnd,
            PAD,
            HEADER_H + PAD,
            W - 2 * PAD,
            edit_h,
            (ES_MULTILINE | ES_AUTOVSCROLL) as u32,
            edit_wndproc,
            brush_color,
        )?;

        text_host.set_margins(6, 6);
        let edit_font = crate::osd::create_font(-15, false, "Segoe UI");
        text_host.set_font(edit_font);
        text_host.set_text(initial);

        let surface_for_color = if theme.surface.a == 255 {
            theme.surface
        } else {
            theme.surface.blend_over(theme.background)
        };
        text_host.set_default_text_color(surface_for_color.contrasting_text_color());

        // ── Allocate state (lives until DescriptionEditor::Drop) ─────
        let state = Box::new(EditorState {
            text_host,
            edit_font,
            theme: theme.clone(),
            label,
            applied: false,
        });
        let state_ptr = Box::into_raw(state);
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
        }

        // ── Position centred on parent ───────────────────────────────
        unsafe {
            let mut parent_rect = RECT::default();
            let _ = GetWindowRect(parent, &mut parent_rect);
            let cx = parent_rect.left + (parent_rect.right - parent_rect.left) / 2;
            let cy = parent_rect.top + (parent_rect.bottom - parent_rect.top) / 2;
            let _ = SetWindowPos(
                hwnd,
                HWND::default(),
                cx - W / 2,
                cy - H / 2,
                W,
                H,
                SWP_NOZORDER,
            );
        }

        // ── Title ────────────────────────────────────────────────────
        let title = format!("Annotation {}\0", label);
        let tw: Vec<u16> = title.encode_utf16().collect();
        unsafe {
            let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(tw.as_ptr()));
        }

        // ── Show ─────────────────────────────────────────────────────
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        }
        // Focus the edit control
        if let Some(s) = Self::state(state_ptr) {
            unsafe { s.text_host.focus(hwnd) };
        }

        Some(DescriptionEditor {
            hwnd,
            state: NonNull::new(state_ptr).unwrap(),
        })
    }

    /// Read the current text from the editor.
    pub fn get_text(&self) -> String {
        unsafe { (*self.state.as_ptr()).text_host.get_text() }
    }

    /// Whether the user clicked Apply (as opposed to Cancel or closing).
    pub fn was_applied(&self) -> bool {
        unsafe { (*self.state.as_ptr()).applied }
    }

    fn state(ptr: *mut EditorState) -> Option<&'static mut EditorState> {
        if ptr.is_null() {
            None
        } else {
            Some(unsafe { &mut *ptr })
        }
    }
}

impl Drop for DescriptionEditor {
    fn drop(&mut self) {
        // Destroy the window if still valid (triggers WM_DESTROY which
        // nulls GWLP_USERDATA but does NOT free the box — we do that here).
        if !self.hwnd.is_invalid() {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
        }
        // Free the boxed EditorState (drops TextHost) and delete the font.
        unsafe {
            let state = Box::from_raw(self.state.as_ptr());
            let _ = DeleteObject(state.edit_font);
        }
    }
}

// ── Window proc ────────────────────────────────────────────────────────

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match std::panic::catch_unwind(|| wndproc_inner(hwnd, msg, wp, lp)) {
        Ok(r) => r,
        Err(_) => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn get_state(hwnd: HWND) -> Option<&'static mut EditorState> {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        if ptr == 0 {
            None
        } else {
            Some(&mut *(ptr as *mut EditorState))
        }
    }
}

fn wndproc_inner(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => {
            let mut pt = POINT {
                x: (lp.0 as i16) as i32,
                y: ((lp.0 >> 16) as i16) as i32,
            };
            unsafe {
                let _ = ScreenToClient(hwnd, &mut pt);
            }
            if pt.y >= 0 && pt.y < HEADER_H {
                return LRESULT(HTCAPTION as isize);
            }
            LRESULT(HTCLIENT as isize)
        }

        WM_APP_APPLY => {
            if let Some(st) = get_state(hwnd) {
                st.applied = true;
            }
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }

        WM_APP_CANCEL => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }

        WM_CLOSE => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }

        WM_DESTROY => {
            // Null the GWLP_USERDATA; the box is freed by
            // DescriptionEditor::Drop, which outlives window destruction.
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                // Notify the parent that the editor is done.
                if let Ok(parent) = GetParent(hwnd) {
                    let _ = PostMessageW(parent, WM_APP_EDITOR_DONE, WPARAM(0), LPARAM(0));
                }
            }
            LRESULT(0)
        }

        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            unsafe {
                let hdc = BeginPaint(hwnd, &mut ps);
                if !hdc.is_invalid() {
                    if let Some(st) = get_state(hwnd) {
                        paint(hwnd, hdc, st);
                    }
                    let _ = EndPaint(hwnd, &ps);
                }
            }
            LRESULT(0)
        }

        WM_CTLCOLOREDIT => {
            if let Some(st) = get_state(hwnd) {
                let hdc = HDC(wp.0 as *mut _);
                unsafe {
                    let _ = SetBkMode(hdc, OPAQUE);
                    let surface = st.theme.surface.blend_over(st.theme.background);
                    let _ = SetBkColor(hdc, surface.to_colorref());
                    let text_color = surface.contrasting_text_color();
                    let _ = SetTextColor(hdc, text_color.to_colorref());
                }
                return LRESULT(st.text_host.brush().0 as isize);
            }
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }

        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

// ── RichEdit subclass ──────────────────────────────────────────────────

extern "system" fn edit_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match std::panic::catch_unwind(|| edit_wndproc_inner(hwnd, msg, wp, lp)) {
        Ok(r) => r,
        Err(_) => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn edit_wndproc_inner(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let old_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        if old_ptr == 0 {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let old_proc: WNDPROC = std::mem::transmute(old_ptr);

        if msg == WM_KEYDOWN {
            let vk = wp.0 as u16;
            if vk == VK_ESCAPE.0 {
                if let Ok(parent) = GetParent(hwnd) {
                    let _ = PostMessageW(parent, WM_APP_CANCEL, WPARAM(0), LPARAM(0));
                }
                return LRESULT(0);
            } else if vk == VK_RETURN.0 {
                let shift = (GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0;
                if shift {
                    return CallWindowProcW(old_proc, hwnd, msg, wp, lp);
                }
                if let Ok(parent) = GetParent(hwnd) {
                    let _ = PostMessageW(parent, WM_APP_APPLY, WPARAM(0), LPARAM(0));
                }
                return LRESULT(0);
            }
        }

        CallWindowProcW(old_proc, hwnd, msg, wp, lp)
    }
}

// ── Painting ───────────────────────────────────────────────────────────

fn paint(_hwnd: HWND, hdc: HDC, st: &EditorState) {
    unsafe {
        let mut rc = RECT::default();
        let _ = GetClientRect(_hwnd, &mut rc);

        // Background
        let bg_b = st.theme.background.blend_over(Argb::new(255, 0, 0, 0));
        let bg = CreateSolidBrush(bg_b.to_colorref());
        let _ = FillRect(hdc, &rc, bg);
        let _ = DeleteObject(bg);

        // Border
        let pen_color = st.theme.border.blend_over(st.theme.background);
        let pen = CreatePen(PS_SOLID, 1, pen_color.to_colorref());
        let old_pen = SelectObject(hdc, pen);
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        let _ = Rectangle(hdc, rc.left, rc.top, rc.right, rc.bottom);

        // Header separator
        let _ = MoveToEx(hdc, rc.left + 1, HEADER_H, None);
        let _ = LineTo(hdc, rc.right - 1, HEADER_H);

        // Edit field border
        let edit_y = HEADER_H + PAD - 1;
        let edit_h = rc.bottom - HEADER_H - BTN_H - PAD * 3 + 1;
        let edit_rc = RECT {
            left: PAD - 1,
            top: edit_y,
            right: rc.right - PAD + 1,
            bottom: edit_y + edit_h,
        };
        let _ = Rectangle(
            hdc,
            edit_rc.left,
            edit_rc.top,
            edit_rc.right,
            edit_rc.bottom,
        );

        let _ = SelectObject(hdc, old_brush);
        let _ = SelectObject(hdc, old_pen);
        let _ = DeleteObject(pen);

        // Cancel and Apply buttons at the bottom
        let btn_bottom = rc.bottom - PAD;
        let btn_top = btn_bottom - BTN_H;
        let btn_right = rc.right - PAD;
        let mut bx = btn_right;

        bx -= BTN_W;
        let apply_rc = RECT {
            left: bx,
            top: btn_top,
            right: bx + BTN_W,
            bottom: btn_bottom,
        };
        draw_button(hdc, &apply_rc, "Apply");

        bx -= BTN_GAP + BTN_W;
        let cancel_rc = RECT {
            left: bx,
            top: btn_top,
            right: bx + BTN_W,
            bottom: btn_bottom,
        };
        draw_button(hdc, &cancel_rc, "Cancel");

        // Title
        let _ = SetBkMode(hdc, TRANSPARENT);
        let title_color = st.theme.background.contrasting_text_color();
        let _ = SetTextColor(hdc, title_color.to_colorref());
        let title_text = format!("Annotation {}", st.label);
        let mut title_rc = RECT {
            left: PAD,
            top: 0,
            right: rc.right - PAD,
            bottom: HEADER_H,
        };
        let mut tw: Vec<u16> = title_text
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let _ = DrawTextW(
            hdc,
            &mut tw,
            &mut title_rc as *mut RECT,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
    }
}

fn draw_button(hdc: HDC, rc: &RECT, text: &str) {
    unsafe {
        let br = CreateSolidBrush(COLORREF(0x33333333u32));
        let _ = FillRect(hdc, rc, br);
        let _ = DeleteObject(br);

        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(0x00DDDDDDu32));
        let mut wz: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let mut btn_rc = *rc;
        let _ = DrawTextW(
            hdc,
            &mut wz,
            &mut btn_rc as *mut RECT,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
    }
}

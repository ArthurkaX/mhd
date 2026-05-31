//! `TextHost` — lightweight RAII wrapper around a native EDIT or RichEdit
//! control.
//!
//! Handles creation, font setup, subclassing, brush creation, text access,
//! and brush cleanup on drop. Each overlay provides its own subclass
//! function for app‑specific key handling (Enter / Escape etc.).
//!
//! The parent window is responsible for `DestroyWindow` (which destroys
//! child windows automatically). `TextHost::drop` only frees the
//! background brush.
//!
//! RichEdit notes
//! ──────────────
//! • `msftedit.dll` is loaded once on first `TextHost::create(RichEdit)` and
//!   stays loaded for the process lifetime.
//! • `EM_SETBKGNDCOLOR` is sent after creation to set the background colour.
//!   For translucent themes the overlay's `SetLayeredWindowAttributes` still
//!   provides uniform window opacity; `EM_SETBKGNDCOLOR` just eliminates the
//!   opaque white rectangle that EDIT draws behind the text.

use std::sync::OnceLock;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, LoadLibraryW};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Controls::RichEdit::{
    CFE_EFFECTS, CFM_COLOR, CHARFORMATW, EM_SETBKGNDCOLOR, EM_SETCHARFORMAT, SCF_ALL, SCF_DEFAULT,
};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use crate::core::native_theme::Argb;

/// Which native text control to create.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TextHostKind {
    /// Standard single-line or multi-line EDIT control.
    Edit,
    /// RichEdit 5.0 (msftedit.dll, `RICHEDIT50W` class).
    /// Supports `EM_SETBKGNDCOLOR` for transparent background.
    RichEdit,
}

/// Wrapper around a native EDIT / RichEdit child control.
pub struct TextHost {
    hwnd: HWND,
    brush: HBRUSH,
}

// Re‑export the subclass signature for convenience.
pub type EditWndProc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

// ── msftedit.dll loader ────────────────────────────────────────────────

/// Ensure `msftedit.dll` is loaded. Safe to call multiple times.
fn ensure_msftedit() -> bool {
    static LOADED: OnceLock<bool> = OnceLock::new();
    *LOADED.get_or_init(|| unsafe { LoadLibraryW(windows::core::w!("msftedit.dll")).is_ok() })
}

/// RichEdit 5.0 class name (not exported by `windows` crate directly).
const MSFTEDIT_CLASS: &str = "RICHEDIT50W";

impl TextHost {
    /// Create an EDIT or RichEdit child control.
    ///
    /// * `kind`      — `TextHostKind::Edit` or `TextHostKind::RichEdit`.
    /// * `parent`    — owning window.
    /// * `edit_style` — additional control styles, e.g. `ES_MULTILINE | ES_AUTOVSCROLL`.
    ///                   `WS_CHILD | WS_VISIBLE` is added automatically.
    /// * `wndproc`   — subclass procedure for app‑specific key handling.
    ///                  The old window proc is stored in `GWLP_USERDATA`.
    /// * `brush_color` — background colour passed to `CreateSolidBrush` for
    ///                   `WM_CTLCOLOREDIT`. The caller's parent wndproc should
    ///                   return this brush.
    pub fn create(
        kind: TextHostKind,
        parent: HWND,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        edit_style: u32,
        wndproc: EditWndProc,
        brush_color: Argb,
    ) -> Option<Self> {
        // Keep the class name wide string alive for the entire function.
        let class_wide = match kind {
            TextHostKind::Edit => None,
            TextHostKind::RichEdit => {
                ensure_msftedit();
                Some(
                    MSFTEDIT_CLASS
                        .encode_utf16()
                        .chain(std::iter::once(0))
                        .collect::<Vec<u16>>(),
                )
            }
        };
        let class_ptr = match kind {
            TextHostKind::Edit => windows::core::w!("EDIT").as_ptr(),
            TextHostKind::RichEdit => class_wide.as_ref().unwrap().as_ptr(),
        };

        let hinst = unsafe { GetModuleHandleW(None) }.ok()?;

        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR::from_raw(class_ptr),
                PCWSTR::null(),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(edit_style),
                x,
                y,
                w,
                h,
                parent,
                HMENU(100usize as *mut _), // arbitrary child ID
                hinst,
                None,
            )
        }
        .ok()?;

        // Default font
        unsafe {
            let _ = SendMessageW(
                hwnd,
                WM_SETFONT,
                WPARAM(GetStockObject(DEFAULT_GUI_FONT).0 as _),
                LPARAM(1),
            );
        }

        // Subclass — store old proc in GWLP_USERDATA (standard Windows pattern)
        unsafe {
            let old_proc = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, wndproc as *const () as isize);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, old_proc);
        }

        // Background brush for WM_CTLCOLOREDIT
        let brush = unsafe { CreateSolidBrush(brush_color.to_colorref()) };

        // For RichEdit, also set the background colour directly so the
        // control doesn't draw a white rectangle before text.
        if kind == TextHostKind::RichEdit {
            unsafe {
                // EM_SETBKGNDCOLOR: wParam = 0 (use lParam as colour),
                // lParam = COLORREF
                let _ = SendMessageW(
                    hwnd,
                    EM_SETBKGNDCOLOR,
                    WPARAM(0),
                    LPARAM(brush_color.to_colorref().0 as isize),
                );
            }
        }

        Some(TextHost { hwnd, brush })
    }

    // ── Text color (RichEdit) ────────────────────────────────────────

    /// Set the default text colour for new and existing content via
    /// `EM_SETCHARFORMAT`.  RichEdit ignores `SetTextColor` from
    /// `WM_CTLCOLOREDIT` — this is the reliable way to control text colour.
    pub fn set_default_text_color(&self, color: Argb) {
        unsafe {
            let cf = CHARFORMATW {
                cbSize: std::mem::size_of::<CHARFORMATW>() as u32,
                dwMask: CFM_COLOR,
                dwEffects: CFE_EFFECTS::default(), // 0 = explicit color (not AUTOCOLOR)
                crTextColor: color.to_colorref(),
                ..Default::default()
            };
            // Apply to both default format (new text) and all existing text.
            let _ = SendMessageW(
                self.hwnd,
                EM_SETCHARFORMAT,
                WPARAM((SCF_DEFAULT | SCF_ALL) as usize),
                LPARAM(&cf as *const _ as isize),
            );
        }
    }

    // ── Font ─────────────────────────────────────────────────────────

    /// Replace the control's font.
    pub fn set_font(&self, font: HFONT) {
        unsafe {
            let _ = SendMessageW(self.hwnd, WM_SETFONT, WPARAM(font.0 as _), LPARAM(1));
        }
    }

    // ── Margins ───────────────────────────────────────────────────────

    /// Set left and right margins (EM_SETMARGINS).
    pub fn set_margins(&self, left: u32, right: u32) {
        unsafe {
            let _ = SendMessageW(
                self.hwnd,
                EM_SETMARGINS,
                WPARAM((EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize),
                LPARAM((left | (right << 16)) as isize),
            );
        }
    }

    // ── Accessors ──────────────────────────────────────────────────────

    /// The control `HWND`.
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// The background `HBRUSH` to return in `WM_CTLCOLOREDIT`.
    pub fn brush(&self) -> HBRUSH {
        self.brush
    }

    // ── Text helpers ───────────────────────────────────────────────────

    /// Read the current text from the control.
    pub fn get_text(&self) -> String {
        get_edit_text(self.hwnd)
    }

    /// Set the text of the control.
    pub fn set_text(&self, text: &str) {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let _ = SendMessageW(
                self.hwnd,
                WM_SETTEXT,
                WPARAM(0),
                LPARAM(wide.as_ptr() as isize),
            );
        }
    }

    // ── Focus ──────────────────────────────────────────────────────────

    /// Steal focus from the current foreground window and give it to the
    /// control (handles cross‑thread focus via `AttachThreadInput`).
    /// # Safety
    ///
    /// Must be called from the thread that owns `self.hwnd`.
    pub unsafe fn focus(&self, parent: HWND) {
        unsafe { steal_focus(parent, self.hwnd) };
    }
}

impl Drop for TextHost {
    fn drop(&mut self) {
        // The parent owns DestroyWindow — it destroys children automatically.
        // We only free the GDI brush.
        if !self.brush.is_invalid() {
            unsafe {
                let _ = DeleteObject(self.brush);
            }
        }
    }
}

// ── Free helpers also used by overlay modules ───────────────────────────

/// Read the full text of an EDIT / RichEdit control.
pub fn get_edit_text(hwnd: HWND) -> String {
    unsafe {
        let len = SendMessageW(hwnd, WM_GETTEXTLENGTH, WPARAM(0), LPARAM(0)).0 as usize;
        if len == 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len + 1];
        let _ = SendMessageW(
            hwnd,
            WM_GETTEXT,
            WPARAM(buf.len()),
            LPARAM(buf.as_mut_ptr() as isize),
        );
        String::from_utf16_lossy(&buf[..len])
    }
}

/// Steal keyboard focus for a child window, attaching input queues if
/// the foreground window belongs to a different thread.
///
/// SAFETY: `parent` and `child` must be valid HWNDs owned by the calling
/// thread.
pub unsafe fn steal_focus(parent: HWND, child: HWND) {
    unsafe {
        let our_tid = GetCurrentThreadId();
        let fore_tid = GetWindowThreadProcessId(GetForegroundWindow(), None);
        if fore_tid != our_tid && fore_tid != 0 {
            let _ = AttachThreadInput(our_tid, fore_tid, true);
            let _ = SetForegroundWindow(parent);
            let _ = AttachThreadInput(our_tid, fore_tid, false);
        }
        let _ = SetFocus(child);
    }
}

// ── Edit control messages not exported by `windows` crate ─────────────

const EM_SETMARGINS: u32 = 0x00D3;

// ── Argb → COLORREF helper (inline to keep the dependency local) ───────

#[allow(dead_code)]
trait ToColorref {
    fn to_colorref(self) -> COLORREF;
}

impl ToColorref for Argb {
    fn to_colorref(self) -> COLORREF {
        COLORREF((self.b as u32) | ((self.g as u32) << 8) | ((self.r as u32) << 16))
    }
}

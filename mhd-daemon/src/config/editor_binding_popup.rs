//! Modal popup for editing a single shortcut binding.
//!
//! Opens when the user clicks a shortcut row in the settings editor.
//! A small layered window with full GDI drawing — no child HWNDs.
//!
//! Layout:
//! ┌─────────────────────────────────────┐
//! │  Edit Shortcut                      │
//! ├─────────────────────────────────────┤
//! │  Trigger            [● Record]      │
//! │  ┌───────────────────────────────┐  │
//! │  │ Ctrl+Shift+F                  │  │
//! │  └───────────────────────────────┘  │
//! │                                     │
//! │  Action              [▼]           │
//! │  ┌───────────────────────────────┐  │
//! │  │ show_volume_mixer             │  │
//! │  └───────────────────────────────┘  │
//! │                                     │
//! │  Parameter           [● Record]    │
//! │  ┌───────────────────────────────┐  │
//! │  │ 5                             │  │
//! │  └───────────────────────────────┘  │
//! │                                     │
//! │       [Cancel]          [Save]      │
//! └─────────────────────────────────────┘

use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::editor_layout::*;
use crate::hook::WM_BINDING_CAPTURED;
use crate::config::editor_state::ButtonStyle;
use crate::config::editor_theme::{
    draw_button, draw_plain_label, draw_readonly_text_field, draw_rounded_border_in_buffer,
    draw_rounded_rect_in_buffer, to_utf16_z,
};
use crate::core::native_theme::{Argb, NativeTheme};
use crate::core::trigger::KeyCombo;

// ── Constants ──────────────────────────────────────────────────────────

const POPUP_WIDTH_BASE: i32 = 440;
const POPUP_HEIGHT_BASE: i32 = 380;
const POPUP_HEADER_HEIGHT_BASE: i32 = 48;
const POPUP_FOOTER_HEIGHT_BASE: i32 = 52;
const POPUP_RADIUS_BASE: f32 = 12.0;
const POPUP_PADDING: i32 = 20;
const POPUP_ROW_HEIGHT: i32 = 32;
const POPUP_LABEL_WIDTH: i32 = 80;
const POPUP_FIELD_HEIGHT: i32 = 30;

// ── Popup state ────────────────────────────────────────────────────────

#[allow(dead_code)]
pub struct BindingPopupState {
    pub hwnd: HWND,
    pub parent_hwnd: HWND,
    pub parent_ptr: *mut crate::config::editor_state::SettingsState,
    pub binding_idx: usize,

    // Edited data (cloned from parent on open)
    pub trigger: String,
    pub kind_idx: usize,
    pub param: String,

    // UI state
    pub theme: NativeTheme,
    pub scale: f32,
    pub win_w: i32,
    pub win_h: i32,
    pub is_recording_trigger: bool,
    pub is_recording_param: bool,
    pub hovered_target: BindingPopupHit,
    pub action_names: Vec<&'static str>,

    // Combo popup
    pub kind_combo_open: bool,

    // Signal the modal loop to exit
    pub should_close: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingPopupHit {
    None,
    TriggerField,
    RecordTrigger,
    KindCombo,
    ParamField,
    RecordParam,
    SaveBtn,
    CancelBtn,
    KindItem(usize),
}

// ── Window registration ───────────────────────────────────────────────

static POPUP_CLASS: std::sync::OnceLock<u16> = std::sync::OnceLock::new();

fn ensure_popup_class() -> u16 {
    *POPUP_CLASS.get_or_init(|| {
        let hinst: HINSTANCE = unsafe {
            HINSTANCE(
                windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
                    .unwrap_or_default()
                    .0,
            )
        };
        let class_name = to_utf16_z("mhd_BindingEditorPopup");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(binding_popup_wndproc),
            cbClsExtra: 0,
            cbWndExtra: std::mem::size_of::<isize>() as i32, // GWLP_USERDATA
            hInstance: hinst,
            hIcon: HICON::default(),
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
            hbrBackground: HBRUSH::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
        };
        unsafe { RegisterClassW(&wc) }
    })
}

// ── Open popup ────────────────────────────────────────────────────────

/// Open the binding editor popup as a modal dialog.
/// Blocks until the user dismisses it (Save or Cancel).
pub fn open_binding_popup(
    parent_hwnd: HWND,
    parent_ptr: *mut crate::config::editor_state::SettingsState,
    binding_idx: usize,
) {
    let state = unsafe { &*parent_ptr };
    let scale = state.layout.scale();

    let win_w = (POPUP_WIDTH_BASE as f32 * scale) as i32;
    let win_h = (POPUP_HEIGHT_BASE as f32 * scale) as i32;

    // Center on parent window
    let mut parent_rc = RECT::default();
    unsafe {
        let _ = GetWindowRect(parent_hwnd, &mut parent_rc);
    }
    let cx = parent_rc.left + (parent_rc.right - parent_rc.left - win_w) / 2;
    let cy = parent_rc.top + (parent_rc.bottom - parent_rc.top - win_h) / 2;

    // Build action name list
    let action_names: Vec<&'static str> = EDITOR_ACTION_NAMES.to_vec();

    let popup_state = Box::new(BindingPopupState {
        hwnd: HWND::default(),
        parent_hwnd,
        parent_ptr,
        binding_idx,
        trigger: state.bindings[binding_idx].trigger.clone(),
        kind_idx: state.bindings[binding_idx].kind_idx,
        param: state.bindings[binding_idx].param.clone(),
        theme: state.theme.clone(),
        scale,
        win_w,
        win_h,
        is_recording_trigger: false,
        is_recording_param: false,
        hovered_target: BindingPopupHit::None,
        action_names,
        kind_combo_open: false,
        should_close: false,
    });
    let popup_ptr = Box::into_raw(popup_state);

    let class_atom = ensure_popup_class();
    let hinst: HINSTANCE = unsafe {
        HINSTANCE(
            windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
                .unwrap_or_default()
                .0,
        )
    };

    // Build class atom (MAKEINTRESOURCEW) for CreateWindowExW
    let class_wz = to_utf16_z("#32770"); // fallback dialog class if atom fails
    let class_ptr = if class_atom != 0 {
        // Cast atom value directly to pointer (MAKEINTRESOURCEW semantics)
        PCWSTR::from_raw(class_atom as *const u16)
    } else {
        PCWSTR::from_raw(class_wz.as_ptr())
    };

    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class_ptr,
            PCWSTR::from_raw(to_utf16_z("Edit Shortcut").as_ptr()),
            WS_POPUP,
            0,
            0,
            win_w,
            win_h,
            None,
            None,
            hinst,
            Some(popup_ptr as *mut c_void),
        )
    }
    .ok();

    let hwnd = match hwnd {
        Some(h) => h,
        None => {
            let _ = unsafe { Box::from_raw(popup_ptr) };
            return;
        }
    };

    // Store state pointer
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, popup_ptr as isize);
    }

    // Disable parent window
    unsafe {
        let _ = EnableWindow(parent_hwnd, false);
    }

    // Position and show the window FIRST, THEN paint content.
    // For WS_EX_LAYERED, UpdateLayeredWindow's pt_dst determines
    // where the content appears — it must match the window position.
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            cx, cy, win_w, win_h,
            SWP_SHOWWINDOW,
        );
    }

    // Initial paint — content appears at the window's current position.
    unsafe {
        paint_binding_popup(hwnd, popup_ptr);
    }

    // Modal message loop - check should_close flag from Save/Cancel
    loop {
        unsafe {
            // Non-blocking peek to avoid race with should_close
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
        if state_ptr == 0 {
            break;
        }
        let should_close = unsafe { (*(state_ptr as *mut BindingPopupState)).should_close };
        if should_close {
            break;
        }

        // Yield to avoid busy-waiting
        unsafe { let _ = WaitMessage(); }
    }

    // Re-enable parent and bring it to front
    unsafe {
        let _ = EnableWindow(parent_hwnd, true);
        let _ = SetForegroundWindow(parent_hwnd);
    }

    // Cleanup window
    if !hwnd.is_invalid() {
        unsafe { DestroyWindow(hwnd).ok(); }
    }
    // Free the popup state box (WM_NCDESTROY no longer frees it)
    unsafe {
        let _ = Box::from_raw(popup_ptr);
    }
}

// ── Paint ─────────────────────────────────────────────────────────────

unsafe fn paint_binding_popup(hwnd: HWND, state_ptr: *mut BindingPopupState) { unsafe {
    let state = &*state_ptr;
    let theme = &state.theme;
    let w = state.win_w;
    let h = state.win_h;
    let scale = state.scale;

    let mut frame = match crate::renderer::DibFrame::new(w, h) {
        Some(f) => f,
        None => return,
    };
    let dib_dc = frame.dc();
    let bits = frame.pixels_mut().as_mut_ptr() as *mut c_void;

    // Background rounded rect
    let radius = (POPUP_RADIUS_BASE * scale) as i32;
    crate::osd::draw_rounded_rect(frame.pixels_mut(), w, h, radius, theme.background);

    let _ = SetBkMode(dib_dc, TRANSPARENT);

    // Fonts
    let title_font = crate::renderer::create_font(
        -(FONT_TITLE_SIZE as f32 * scale) as i32,
        true,
        "Segoe UI Variable",
    );
    let body_font = crate::renderer::create_font(
        -(FONT_BODY_SIZE as f32 * scale) as i32,
        false,
        "Segoe UI Variable",
    );
    let small_font = crate::renderer::create_font(
        -(FONT_SMALL_SIZE as f32 * scale) as i32,
        false,
        "Segoe UI Variable",
    );
    let old_font = SelectObject(dib_dc, title_font);

    // ── Header title ──────────────────────────────────────────
    SetTextColor(dib_dc, theme.text.to_colorref());
    let header_h = (POPUP_HEADER_HEIGHT_BASE as f32 * scale) as i32;
    let pad = (POPUP_PADDING as f32 * scale) as i32;
    let mut title_wz = to_utf16_z("Edit Shortcut");
    let mut title_rc = RECT {
        left: pad,
        top: pad / 2,
        right: w - pad,
        bottom: pad / 2 + 18 + 4,
    };
    DrawTextW(
        dib_dc,
        &mut title_wz,
        &mut title_rc,
        DT_LEFT | DT_SINGLELINE,
    );

    // Separator under header
    let sep_brush = CreateSolidBrush(theme.border.to_colorref());
    FillRect(
        dib_dc,
        &RECT {
            left: pad,
            top: header_h - 1,
            right: w - pad,
            bottom: header_h,
        },
        sep_brush,
    );

    // ── Content layout ─────────────────────────────────────────
    let field_h = (POPUP_FIELD_HEIGHT as f32 * scale) as i32;
    let row_h = (POPUP_ROW_HEIGHT as f32 * scale) as i32;
    let label_w = (POPUP_LABEL_WIDTH as f32 * scale) as i32;
    let content_y = header_h + (8.0 * scale) as i32;

    // --- Trigger row ---
    let trigger_label_y = content_y;

    // Label
    SelectObject(dib_dc, body_font);
    draw_plain_label(
        dib_dc,
        RECT {
            left: pad,
            top: trigger_label_y,
            right: pad + label_w,
            bottom: trigger_label_y + row_h,
        },
        "Trigger",
        body_font,
        theme.text,
    );

    // Trigger field (read-only, shows current trigger)
    let trigger_field_rect = RECT {
        left: pad + label_w + 8,
        top: trigger_label_y,
        right: w - pad - field_h - 8,
        bottom: trigger_label_y + field_h,
    };
    let is_trigger_hovered = state.hovered_target == BindingPopupHit::TriggerField
        || state.hovered_target == BindingPopupHit::RecordTrigger;
    let field_bg = if is_trigger_hovered {
        theme
            .hover
            .blend_over(theme.surface.blend_over(theme.background))
    } else {
        theme.surface.blend_over(theme.background)
    };
    draw_readonly_text_field(
        dib_dc,
        bits,
        w,
        h,
        trigger_field_rect,
        &state.trigger,
        None,
        body_font,
        field_bg,
        theme.border,
        theme.text,
    );

    // Record button (right of trigger field)
    let record_btn_rect = RECT {
        left: w - pad - field_h,
        top: trigger_label_y,
        right: w - pad,
        bottom: trigger_label_y + field_h,
    };
    let is_record_hovered = state.hovered_target == BindingPopupHit::RecordTrigger;
    let record_bg = if state.is_recording_trigger {
        Argb::new(255, 200, 50, 50)
    } else if is_record_hovered {
        theme
            .hover
            .blend_over(theme.surface.blend_over(theme.background))
    } else {
        theme.surface.blend_over(theme.background)
    };
    draw_rounded_rect_in_buffer(bits, w, h, record_btn_rect, (4.0 * scale) as i32, record_bg);
    SetTextColor(dib_dc, theme.text.to_colorref());
    SelectObject(dib_dc, small_font);
    let mut rec_wz = to_utf16_z(if state.is_recording_trigger {
        "●"
    } else {
        "○"
    });
    let mut rec_rc = record_btn_rect;
    DrawTextW(
        dib_dc,
        &mut rec_wz,
        &mut rec_rc,
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );
    draw_rounded_border_in_buffer(
        bits,
        w,
        h,
        record_btn_rect,
        (4.0 * scale) as i32,
        1,
        theme.border,
    );

    // --- Action row ---
    let action_label_y = trigger_label_y + row_h + (4.0 * scale) as i32;
    SelectObject(dib_dc, body_font);
    draw_plain_label(
        dib_dc,
        RECT {
            left: pad,
            top: action_label_y,
            right: pad + label_w,
            bottom: action_label_y + row_h,
        },
        "Action",
        body_font,
        theme.text,
    );

    // Action kind combo
    let combo_w = (200.0 * scale) as i32;
    let combo_rect = RECT {
        left: pad + label_w + 8,
        top: action_label_y,
        right: pad + label_w + 8 + combo_w,
        bottom: action_label_y + field_h,
    };
    let is_combo_hovered = state.hovered_target == BindingPopupHit::KindCombo;
    let combo_bg = if is_combo_hovered {
        theme
            .hover
            .blend_over(theme.surface.blend_over(theme.background))
    } else {
        theme.surface.blend_over(theme.background)
    };
    draw_rounded_rect_in_buffer(bits, w, h, combo_rect, (4.0 * scale) as i32, combo_bg);
    draw_rounded_border_in_buffer(
        bits,
        w,
        h,
        combo_rect,
        (4.0 * scale) as i32,
        1,
        if is_combo_hovered {
            theme.text
        } else {
            theme.border
        },
    );

    let kind_name = state
        .action_names
        .get(state.kind_idx)
        .copied()
        .unwrap_or("quit");
    SelectObject(dib_dc, body_font);
    SetTextColor(dib_dc, theme.text.to_colorref());
    let mut kind_wz = to_utf16_z(kind_name);
    let mut kind_rc = RECT {
        left: combo_rect.left + (6.0 * scale) as i32,
        top: combo_rect.top,
        right: combo_rect.right - (6.0 * scale) as i32,
        bottom: combo_rect.bottom,
    };
    DrawTextW(
        dib_dc,
        &mut kind_wz,
        &mut kind_rc,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );

    // Dropdown arrow
    SetTextColor(dib_dc, theme.text_muted.to_colorref());
    let mut arrow_wz = to_utf16_z("▼");
    let mut arrow_rc = RECT {
        left: combo_rect.right - field_h,
        top: combo_rect.top,
        right: combo_rect.right,
        bottom: combo_rect.bottom,
    };
    DrawTextW(
        dib_dc,
        &mut arrow_wz,
        &mut arrow_rc,
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );

    // --- Parameter row ---
    let param_label_y = action_label_y + row_h + (4.0 * scale) as i32;
    SelectObject(dib_dc, body_font);
    draw_plain_label(
        dib_dc,
        RECT {
            left: pad,
            top: param_label_y,
            right: pad + label_w,
            bottom: param_label_y + row_h,
        },
        "Parameter",
        body_font,
        theme.text,
    );

    // Parameter field
    let param_field_rect = RECT {
        left: pad + label_w + 8,
        top: param_label_y,
        right: w - pad - field_h - 8,
        bottom: param_label_y + field_h,
    };
    let is_param_hovered = state.hovered_target == BindingPopupHit::ParamField
        || state.hovered_target == BindingPopupHit::RecordParam;
    let param_bg = if is_param_hovered {
        theme
            .hover
            .blend_over(theme.surface.blend_over(theme.background))
    } else {
        theme.surface.blend_over(theme.background)
    };
    let param_text = if state.param.is_empty() && !state.is_recording_param {
        "(click ● to record)"
    } else {
        &state.param
    };
    draw_readonly_text_field(
        dib_dc,
        bits,
        w,
        h,
        param_field_rect,
        param_text,
        None,
        body_font,
        param_bg,
        theme.border,
        theme.text_muted,
    );

    // Record button for param
    let param_rec_rect = RECT {
        left: w - pad - field_h,
        top: param_label_y,
        right: w - pad,
        bottom: param_label_y + field_h,
    };
    let is_param_rec_hovered = state.hovered_target == BindingPopupHit::RecordParam;
    let param_rec_bg = if state.is_recording_param {
        Argb::new(255, 200, 50, 50)
    } else if is_param_rec_hovered {
        theme
            .hover
            .blend_over(theme.surface.blend_over(theme.background))
    } else {
        theme.surface.blend_over(theme.background)
    };
    draw_rounded_rect_in_buffer(
        bits,
        w,
        h,
        param_rec_rect,
        (4.0 * scale) as i32,
        param_rec_bg,
    );
    SetTextColor(dib_dc, theme.text.to_colorref());
    SelectObject(dib_dc, small_font);
    let mut rec2_wz = to_utf16_z(if state.is_recording_param {
        "●"
    } else {
        "○"
    });
    let mut rec2_rc = param_rec_rect;
    DrawTextW(
        dib_dc,
        &mut rec2_wz,
        &mut rec2_rc,
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );
    draw_rounded_border_in_buffer(
        bits,
        w,
        h,
        param_rec_rect,
        (4.0 * scale) as i32,
        1,
        theme.border,
    );

    // ── Footer separator ──────────────────────────────────────
    let footer_h = (POPUP_FOOTER_HEIGHT_BASE as f32 * scale) as i32;
    let footer_y = h - footer_h;
    FillRect(
        dib_dc,
        &RECT {
            left: pad,
            top: footer_y,
            right: w - pad,
            bottom: footer_y + 1,
        },
        sep_brush,
    );

    // ── Buttons (right to left: Cancel, Save) ─────────────────
    let btn_w = (90.0 * scale) as i32;
    let btn_hh = (30.0 * scale) as i32;
    let btn_y = footer_y + (footer_h - btn_hh) / 2;
    let save_x = w - pad - btn_w;
    let cancel_x = save_x - btn_w - (8.0 * scale) as i32;

    let is_cancel_hovered = state.hovered_target == BindingPopupHit::CancelBtn;
    draw_button(
        dib_dc,
        bits,
        w,
        h,
        cancel_x,
        btn_y,
        btn_w,
        btn_hh,
        "Cancel",
        theme,
        body_font,
        is_cancel_hovered,
        ButtonStyle::Secondary,
    );

    let is_save_hovered = state.hovered_target == BindingPopupHit::SaveBtn;
    draw_button(
        dib_dc,
        bits,
        w,
        h,
        save_x,
        btn_y,
        btn_w,
        btn_hh,
        "Save",
        theme,
        body_font,
        is_save_hovered,
        ButtonStyle::Primary,
    );

    // ── Cleanup ────────────────────────────────────────────────
    let _ = DeleteObject(sep_brush);
    SelectObject(dib_dc, old_font);
    let _ = DeleteObject(title_font);
    let _ = DeleteObject(body_font);
    let _ = DeleteObject(small_font);

    frame.fix_gdi_alpha(theme.background);

    let cur_pos = {
        let mut wr = RECT::default();
        let _ = GetWindowRect(hwnd, &mut wr);
        (wr.left, wr.top)
    };
    frame.present_layered(hwnd, cur_pos.0, cur_pos.1, 255);
}}

// ── Helper: resolve captured key to string ────────────────────────────

fn key_to_string(data: usize) -> String {
    let mods = crate::core::trigger::Modifiers((data & 0xFF) as u8);
    let key_type = (data >> 8) & 0xFF;
    let key_val = (data >> 16) & 0xFF;

    let physical_key = if key_type == 0 {
        Some(crate::core::trigger::PhysicalKey::Keyboard(key_val as u8))
    } else if key_type == 1 {
        Some(crate::core::trigger::PhysicalKey::MouseButton(
            key_val as u8,
        ))
    } else {
        match key_val {
            0 => Some(crate::core::trigger::PhysicalKey::WheelUp),
            1 => Some(crate::core::trigger::PhysicalKey::WheelDown),
            2 => Some(crate::core::trigger::PhysicalKey::WheelLeft),
            3 => Some(crate::core::trigger::PhysicalKey::WheelRight),
            _ => None,
        }
    };

    let kc = KeyCombo {
        modifiers: mods,
        key: physical_key,
    };

    crate::core::trigger::keys_to_string(&kc)
}

// ── Window procedure ─────────────────────────────────────────────────

unsafe extern "system" fn binding_popup_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT { unsafe {
    match msg {
        WM_CREATE => LRESULT(0),

        WM_NCHITTEST => {
            let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut BindingPopupState;
            if state_ptr.is_null() {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            let state = &*state_ptr;
            let screen_x = (lparam.0 as i16) as i32;
            let screen_y = ((lparam.0 >> 16) as i16) as i32;
            let mut pt = POINT {
                x: screen_x,
                y: screen_y,
            };
            let _ = ScreenToClient(hwnd, &mut pt);

            let header_h = (POPUP_HEADER_HEIGHT_BASE as f32 * state.scale) as i32;
            if pt.y < header_h {
                return LRESULT(HTCAPTION as isize);
            }
            LRESULT(HTCLIENT as isize)
        }

        WM_PAINT => {
            let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut BindingPopupState;
            if !state_ptr.is_null() {
                paint_binding_popup(hwnd, state_ptr);
            }
            LRESULT(0)
        }

        WM_LBUTTONDOWN => {
            let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut BindingPopupState;
            if state_ptr.is_null() {
                return LRESULT(0);
            }
            let state = &mut *state_ptr;
            let x = (lparam.0 as i16) as i32;
            let y = ((lparam.0 >> 16) as i16) as i32;
            let hit = hit_test_popup(state, x, y);

            match hit {
                BindingPopupHit::RecordTrigger => {
                    state.is_recording_trigger = !state.is_recording_trigger;
                    state.is_recording_param = false;
                    if state.is_recording_trigger {
                        crate::hook::set_recording_window(Some(state.parent_hwnd));
                    } else {
                        crate::hook::set_recording_window(None);
                    }
                    paint_binding_popup(hwnd, state_ptr);
                }
                BindingPopupHit::KindCombo => {
                    state.kind_combo_open = !state.kind_combo_open;
                    paint_binding_popup(hwnd, state_ptr);
                }
                BindingPopupHit::RecordParam => {
                    state.is_recording_param = !state.is_recording_param;
                    state.is_recording_trigger = false;
                    if state.is_recording_param {
                        crate::hook::set_recording_window(Some(state.parent_hwnd));
                    } else {
                        crate::hook::set_recording_window(None);
                    }
                    paint_binding_popup(hwnd, state_ptr);
                }
                BindingPopupHit::SaveBtn => {
                    // Write changes back to parent state
                    let parent = &mut *state.parent_ptr;
                    if state.binding_idx < parent.bindings.len() {
                        parent.bindings[state.binding_idx].trigger = state.trigger.clone();
                        parent.bindings[state.binding_idx].kind_idx = state.kind_idx;
                        parent.bindings[state.binding_idx].param = state.param.clone();
                    }
                    crate::hook::set_recording_window(None);
                    state.should_close = true;
                }
                BindingPopupHit::CancelBtn => {
                    crate::hook::set_recording_window(None);
                    state.should_close = true;
                }
                BindingPopupHit::KindItem(idx) => {
                    state.kind_idx = idx;
                    state.kind_combo_open = false;
                    paint_binding_popup(hwnd, state_ptr);
                }
                _ => {}
            }
            LRESULT(0)
        }

        WM_MOUSEMOVE => {
            let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut BindingPopupState;
            if state_ptr.is_null() {
                return LRESULT(0);
            }
            let state = &mut *state_ptr;
            let x = (lparam.0 as i16) as i32;
            let y = ((lparam.0 >> 16) as i16) as i32;
            let target = hit_test_popup(state, x, y);
            if state.hovered_target != target {
                state.hovered_target = target;
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
                paint_binding_popup(hwnd, state_ptr);
            }
            LRESULT(0)
        }

        WM_MOUSELEAVE => {
            let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut BindingPopupState;
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                if state.hovered_target != BindingPopupHit::None {
                    state.hovered_target = BindingPopupHit::None;
                    paint_binding_popup(hwnd, state_ptr);
                }
            }
            LRESULT(0)
        }

        WM_LBUTTONUP => LRESULT(0),

        WM_DESTROY | WM_NCDESTROY => {
            let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut BindingPopupState;
            if !state_ptr.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            LRESULT(0)
        }

        WM_BINDING_CAPTURED => {
            let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut BindingPopupState;
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                let data = lparam.0 as usize;
                let key_str = key_to_string(data);

                if state.is_recording_trigger {
                    state.trigger = key_str;
                    state.is_recording_trigger = false;
                    crate::hook::set_recording_window(None);
                    paint_binding_popup(hwnd, state_ptr);
                } else if state.is_recording_param {
                    state.param = key_str;
                    state.is_recording_param = false;
                    crate::hook::set_recording_window(None);
                    paint_binding_popup(hwnd, state_ptr);
                }
            }
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
}

// ── Hit-test ──────────────────────────────────────────────────────────

fn hit_test_popup(state: &BindingPopupState, x: i32, y: i32) -> BindingPopupHit {
    let scale = state.scale;
    let pad = (POPUP_PADDING as f32 * scale) as i32;
    let field_h = (POPUP_FIELD_HEIGHT as f32 * scale) as i32;
    let row_h = (POPUP_ROW_HEIGHT as f32 * scale) as i32;
    let header_h = (POPUP_HEADER_HEIGHT_BASE as f32 * scale) as i32;
    let w = state.win_w;
    let h = state.win_h;
    let content_y = header_h + (8.0 * scale) as i32;
    let label_w = (POPUP_LABEL_WIDTH as f32 * scale) as i32;
    let field_x = pad + label_w + 8;
    let combo_w = (200.0 * scale) as i32;

    let trigger_label_y = content_y;
    let action_label_y = trigger_label_y + row_h + (4.0 * scale) as i32;
    let param_label_y = action_label_y + row_h + (4.0 * scale) as i32;

    // Footer buttons
    let footer_h = (POPUP_FOOTER_HEIGHT_BASE as f32 * scale) as i32;
    let footer_y = h - footer_h;
    let btn_w = (90.0 * scale) as i32;
    let btn_h = (30.0 * scale) as i32;
    let btn_y = footer_y + (footer_h - btn_h) / 2;
    let save_x = w - pad - btn_w;
    let cancel_x = save_x - btn_w - (8.0 * scale) as i32;

    if y >= btn_y && y < btn_y + btn_h {
        if x >= save_x && x < save_x + btn_w {
            return BindingPopupHit::SaveBtn;
        }
        if x >= cancel_x && x < cancel_x + btn_w {
            return BindingPopupHit::CancelBtn;
        }
    }

    // Kind combo popup items (if open)
    if state.kind_combo_open
        && let Some(idx) = hover_kind_item(state, x, y) {
            return BindingPopupHit::KindItem(idx);
        }

    // Record trigger button
    let rec_btn_rect = RECT {
        left: w - pad - field_h,
        top: trigger_label_y,
        right: w - pad,
        bottom: trigger_label_y + field_h,
    };
    if x >= rec_btn_rect.left
        && x < rec_btn_rect.right
        && y >= rec_btn_rect.top
        && y < rec_btn_rect.bottom
    {
        return BindingPopupHit::RecordTrigger;
    }

    // Trigger field
    let trigger_field_rect = RECT {
        left: field_x,
        top: trigger_label_y,
        right: w - pad - field_h - 8,
        bottom: trigger_label_y + field_h,
    };
    if x >= trigger_field_rect.left
        && x < trigger_field_rect.right
        && y >= trigger_field_rect.top
        && y < trigger_field_rect.bottom
    {
        return BindingPopupHit::TriggerField;
    }

    // Kind combo
    let combo_rect = RECT {
        left: field_x,
        top: action_label_y,
        right: field_x + combo_w,
        bottom: action_label_y + field_h,
    };
    if x >= combo_rect.left && x < combo_rect.right && y >= combo_rect.top && y < combo_rect.bottom
    {
        return BindingPopupHit::KindCombo;
    }

    // Record param button
    let param_rec_rect = RECT {
        left: w - pad - field_h,
        top: param_label_y,
        right: w - pad,
        bottom: param_label_y + field_h,
    };
    if x >= param_rec_rect.left
        && x < param_rec_rect.right
        && y >= param_rec_rect.top
        && y < param_rec_rect.bottom
    {
        return BindingPopupHit::RecordParam;
    }

    // Param field
    let param_field_rect = RECT {
        left: field_x,
        top: param_label_y,
        right: w - pad - field_h - 8,
        bottom: param_label_y + field_h,
    };
    if x >= param_field_rect.left
        && x < param_field_rect.right
        && y >= param_field_rect.top
        && y < param_field_rect.bottom
    {
        return BindingPopupHit::ParamField;
    }

    BindingPopupHit::None
}

fn hover_kind_item(state: &BindingPopupState, x: i32, y: i32) -> Option<usize> {
    if !state.kind_combo_open {
        return None;
    }
    let scale = state.scale;
    let pad = (POPUP_PADDING as f32 * scale) as i32;
    let field_h = (POPUP_FIELD_HEIGHT as f32 * scale) as i32;
    let row_h = (POPUP_ROW_HEIGHT as f32 * scale) as i32;
    let header_h = (POPUP_HEADER_HEIGHT_BASE as f32 * scale) as i32;
    let content_y = header_h + (8.0 * scale) as i32;
    let label_w = (POPUP_LABEL_WIDTH as f32 * scale) as i32;
    let field_x = pad + label_w + 8;
    let combo_w = (200.0 * scale) as i32;

    let trigger_label_y = content_y;
    let action_label_y =
        trigger_label_y + row_h + (4.0 * scale) as i32 + row_h + (4.0 * scale) as i32;
    let combo_rect = RECT {
        left: field_x,
        top: action_label_y,
        right: field_x + combo_w,
        bottom: action_label_y + field_h,
    };

    // Popup list below combo
    let item_h = (24.0 * scale) as i32;
    let max_visible = 10;
    let n = state.action_names.len().min(max_visible);
    let popup_rect = RECT {
        left: combo_rect.left,
        top: combo_rect.bottom,
        right: combo_rect.right,
        bottom: combo_rect.bottom + n as i32 * item_h,
    };
    if x >= popup_rect.left && x < popup_rect.right && y >= popup_rect.top && y < popup_rect.bottom
    {
        let idx = (y - popup_rect.top) / item_h;
        if (idx as usize) < state.action_names.len() {
            return Some(idx as usize);
        }
    }
    None
}

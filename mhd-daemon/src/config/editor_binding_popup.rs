//! Modal popup for editing a single shortcut binding.
//!
//! Opens when the user clicks a shortcut row in the settings editor.
//! A small layered window with full GDI drawing — no child HWNDs.
//!
//! The Parameter row adapts to the selected action's param schema:
//!
//!   • ActionParamSchema::None        → row hidden entirely
//!   • ActionParamSchema::Text         → text field, no side button
//!   • ActionParamSchema::FilePath     → text field + [Browse…]
//!   • ActionParamSchema::KeyMapping   → text field + [Bind]
//!   • ActionParamSchema::Number       → text field, no side button
//!   • ActionParamSchema::PowerAction  → row hidden (no param_key)
//!
//! Layout (example with KeyMapping):
//! ┌─────────────────────────────────────┐
//! │  Edit Shortcut                      │
//! ├─────────────────────────────────────┤
//! │  Trigger              [Bind]        │
//! │  ┌───────────────────────────────┐  │
//! │  │ Ctrl+Shift+F                  │  │
//! │  └───────────────────────────────┘  │
//! │                                     │
//! │  Action              [▼]           │
//! │  ┌───────────────────────────────┐  │
//! │  │ show_volume_mixer             │  │
//! │  └───────────────────────────────┘  │
//! │                                     │
//! │  Parameter            [Bind]        │
//! │  ┌───────────────────────────────┐  │
//! │  │ 5                             │  │
//! │  └───────────────────────────────┘  │
//! │                                     │
//! │       [Cancel]          [Save]      │
//! └─────────────────────────────────────┘

use std::ffi::c_void;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use crate::config::editor_key_combo::{KeyComboEditorState, KeyComboSlot};
use crate::config::editor_layout::*;
use crate::config::editor_search_dropdown::{SearchDropdownItem, SearchDropdownState};
use crate::config::editor_state::ButtonStyle;
use crate::config::editor_theme::{
    draw_button, draw_plain_label, draw_rounded_border_in_buffer, draw_rounded_rect_in_buffer,
    to_utf16_z,
};
use crate::core::action::ActionParamSchema;
use crate::core::native_theme::{Argb, NativeTheme};
use crate::core::trigger::KeyCombo;
use crate::hook::WM_BINDING_CAPTURED;

// ── Constants ──────────────────────────────────────────────────────────

const POPUP_WIDTH_BASE: i32 = 640;
const POPUP_HEIGHT_BASE: i32 = 380;
const POPUP_HEADER_HEIGHT_BASE: i32 = 48;
const POPUP_FOOTER_HEIGHT_BASE: i32 = 52;
const POPUP_RADIUS_BASE: f32 = 12.0;
const POPUP_PADDING: i32 = 20;
const POPUP_ROW_HEIGHT: i32 = 32;
const POPUP_LABEL_WIDTH: i32 = 80;
const POPUP_FIELD_HEIGHT: i32 = 30;
const POPUP_BIND_BUTTON_WIDTH: i32 = 76;

// ── Popup state ────────────────────────────────────────────────────────

#[allow(dead_code)]
pub struct BindingPopupState {
    pub hwnd: HWND,
    pub parent_hwnd: HWND,
    pub parent_ptr: *mut crate::config::editor_state::SettingsState,
    pub binding_idx: usize,

    // Edited data (cloned from parent on open)
    pub trigger: String,
    pub trigger_editor: KeyComboEditorState,
    pub kind_idx: usize,
    pub param: String,

    // Key-combo editor used when schema is ActionParamSchema::KeyMapping
    pub param_editor: KeyComboEditorState,

    // UI state
    pub theme: NativeTheme,
    pub scale: f32,
    pub win_w: i32,
    pub win_h: i32,
    pub is_recording_trigger: bool,
    pub is_recording_param: bool,
    pub is_editing_param: bool,
    pub param_edit_cursor: usize,
    pub param_edit_old: String,
    pub param_save_error: Option<String>,
    pub hovered_target: BindingPopupHit,
    pub action_items: Vec<SearchDropdownItem>,

    // Combo popup
    pub action_dropdown: SearchDropdownState,

    // Signal the modal loop to exit
    pub should_close: bool,
    pub saved: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingPopupHit {
    None,
    TriggerSlot(KeyComboSlot),
    TriggerKeyItem(usize),
    RecordTrigger,
    KindCombo,
    ParamSlot(KeyComboSlot),
    ParamKeyItem(usize),
    ParamField,
    RecordParam,
    BrowseParam,
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
) -> bool {
    let state = unsafe { &*parent_ptr };
    if binding_idx >= state.bindings.len() {
        return false;
    }
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

    // Build reusable search-dropdown items. The id is the editor action index.
    let action_items: Vec<SearchDropdownItem> = EDITOR_ACTION_NAMES
        .iter()
        .enumerate()
        .map(|(idx, name)| {
            let desc = editor_action_desc(idx);
            SearchDropdownItem::new(
                idx,
                desc.label,
                vec![name.to_string(), desc.category.label().to_string()],
            )
            .with_description(desc.description)
        })
        .collect();

    let popup_state = Box::new(BindingPopupState {
        hwnd: HWND::default(),
        parent_hwnd,
        parent_ptr,
        binding_idx,
        trigger: state.bindings[binding_idx].trigger.clone(),
        trigger_editor: KeyComboEditorState::from_trigger_string(
            &state.bindings[binding_idx].trigger,
        ),
        kind_idx: state.bindings[binding_idx].kind_idx,
        param: state.bindings[binding_idx].param.clone(),
        param_editor: KeyComboEditorState::from_trigger_string(&state.bindings[binding_idx].param),
        theme: state.theme.clone(),
        scale,
        win_w,
        win_h,
        is_recording_trigger: false,
        is_recording_param: false,
        is_editing_param: false,
        param_edit_cursor: 0,
        param_edit_old: String::new(),
        param_save_error: None,
        hovered_target: BindingPopupHit::None,
        action_items,
        action_dropdown: SearchDropdownState::default(),
        should_close: false,
        saved: false,
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
            return false;
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
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, cx, cy, win_w, win_h, SWP_SHOWWINDOW);
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
        unsafe {
            let _ = WaitMessage();
        }
    }

    // Re-enable parent and bring it to front
    unsafe {
        let _ = EnableWindow(parent_hwnd, true);
        let _ = SetForegroundWindow(parent_hwnd);
    }

    let saved = unsafe { (*popup_ptr).saved };

    // Cleanup window
    if !hwnd.is_invalid() {
        unsafe {
            DestroyWindow(hwnd).ok();
        }
    }
    // Free the popup state box (WM_NCDESTROY no longer frees it)
    unsafe {
        let _ = Box::from_raw(popup_ptr);
    }
    saved
}

// ── Paint ─────────────────────────────────────────────────────────────

unsafe fn paint_binding_popup(hwnd: HWND, state_ptr: *mut BindingPopupState) {
    unsafe {
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

        // Popup background — use a slightly distinct shade from the
        // main window so the popup is visually distinguishable even in
        // themes where background and surface are near-identical.
        let popup_bg = theme.surface.blend_over(theme.background);
        let radius = (POPUP_RADIUS_BASE * scale) as i32;
        crate::osd::draw_rounded_rect(frame.pixels_mut(), w, h, radius, popup_bg);

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
        let bind_btn_w = (POPUP_BIND_BUTTON_WIDTH as f32 * scale) as i32;
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

        // Trigger combo slots
        let trigger_field_rect = RECT {
            left: pad + label_w + 8,
            top: trigger_label_y,
            right: w - pad - bind_btn_w - 8,
            bottom: trigger_label_y + field_h,
        };
        for (i, (slot, slot_rect)) in trigger_slot_rects(trigger_field_rect, scale)
            .iter()
            .enumerate()
        {
            let is_hovered = state.hovered_target == BindingPopupHit::TriggerSlot(*slot);
            let is_open = state.trigger_editor.open_slot == Some(*slot);
            let slot_bg = if is_hovered || is_open {
                theme
                    .hover
                    .blend_over(theme.surface.blend_over(theme.background))
            } else {
                theme.surface.blend_over(theme.background)
            };
            draw_rounded_rect_in_buffer(bits, w, h, *slot_rect, (4.0 * scale) as i32, slot_bg);
            draw_rounded_border_in_buffer(
                bits,
                w,
                h,
                *slot_rect,
                (4.0 * scale) as i32,
                1,
                if is_open { theme.text } else { theme.border },
            );

            SelectObject(dib_dc, small_font);
            SetTextColor(dib_dc, theme.text.to_colorref());
            let mut label_wz = to_utf16_z(&state.trigger_editor.slot_label(*slot));
            let mut label_rect = RECT {
                left: slot_rect.left + (6.0 * scale) as i32,
                top: slot_rect.top,
                right: slot_rect.right - (6.0 * scale) as i32,
                bottom: slot_rect.bottom,
            };
            DrawTextW(
                dib_dc,
                &mut label_wz,
                &mut label_rect,
                DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            );

            if i < 3 {
                SetTextColor(dib_dc, theme.text_muted.to_colorref());
                let mut plus_wz = to_utf16_z("+");
                let next_left = trigger_slot_rects(trigger_field_rect, scale)[i + 1].1.left;
                let mut plus_rect = RECT {
                    left: slot_rect.right,
                    top: slot_rect.top,
                    right: next_left,
                    bottom: slot_rect.bottom,
                };
                DrawTextW(
                    dib_dc,
                    &mut plus_wz,
                    &mut plus_rect,
                    DT_CENTER | DT_SINGLELINE | DT_VCENTER,
                );
            }
        }

        // Record button (right of trigger field)
        let record_btn_rect = RECT {
            left: w - pad - bind_btn_w,
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
        SetTextColor(dib_dc, record_bg.contrasting_text_color().to_colorref());
        SelectObject(dib_dc, small_font);
        let mut rec_wz = to_utf16_z(if state.is_recording_trigger {
            "Listening"
        } else {
            "Bind"
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

        let kind_name = editor_action_desc(state.kind_idx).label;
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

        // --- Parameter row (only for actions with a parameter) ---
        let desc = editor_action_desc(state.kind_idx);
        let param_schema = desc.param_schema;
        let param_label_y = action_label_y + row_h + (4.0 * scale) as i32;

        if desc.param_key.is_some() {
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

            match param_schema {
                ActionParamSchema::KeyMapping => {
                    // ── KeyMapping: slot-based editor (like Trigger) ──
                    let param_key_field_rect = RECT {
                        left: pad + label_w + 8,
                        top: param_label_y,
                        right: w - pad - bind_btn_w - 8,
                        bottom: param_label_y + field_h,
                    };
                    for (i, (slot, slot_rect)) in trigger_slot_rects(param_key_field_rect, scale)
                        .iter()
                        .enumerate()
                    {
                        let is_hovered = state.hovered_target == BindingPopupHit::ParamSlot(*slot);
                        let is_open = state.param_editor.open_slot == Some(*slot);
                        let slot_bg = if is_hovered || is_open {
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
                            *slot_rect,
                            (4.0 * scale) as i32,
                            slot_bg,
                        );
                        draw_rounded_border_in_buffer(
                            bits,
                            w,
                            h,
                            *slot_rect,
                            (4.0 * scale) as i32,
                            1,
                            if is_open { theme.text } else { theme.border },
                        );

                        SelectObject(dib_dc, small_font);
                        SetTextColor(dib_dc, theme.text.to_colorref());
                        let mut label_wz = to_utf16_z(&state.param_editor.slot_label(*slot));
                        let mut label_rect = RECT {
                            left: slot_rect.left + (6.0 * scale) as i32,
                            top: slot_rect.top,
                            right: slot_rect.right - (6.0 * scale) as i32,
                            bottom: slot_rect.bottom,
                        };
                        DrawTextW(
                            dib_dc,
                            &mut label_wz,
                            &mut label_rect,
                            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                        );

                        if i < 3 {
                            SetTextColor(dib_dc, theme.text_muted.to_colorref());
                            let mut plus_wz = to_utf16_z("+");
                            let next_left = trigger_slot_rects(param_key_field_rect, scale)[i + 1]
                                .1
                                .left;
                            let mut plus_rect = RECT {
                                left: slot_rect.right,
                                top: slot_rect.top,
                                right: next_left,
                                bottom: slot_rect.bottom,
                            };
                            DrawTextW(
                                dib_dc,
                                &mut plus_wz,
                                &mut plus_rect,
                                DT_CENTER | DT_SINGLELINE | DT_VCENTER,
                            );
                        }
                    }

                    // Bind button (right of param key field)
                    let param_bind_rect = RECT {
                        left: w - pad - bind_btn_w,
                        top: param_label_y,
                        right: w - pad,
                        bottom: param_label_y + field_h,
                    };
                    let is_param_bind_hovered =
                        state.hovered_target == BindingPopupHit::RecordParam;
                    let param_bind_bg = if state.is_recording_param {
                        Argb::new(255, 200, 50, 50)
                    } else if is_param_bind_hovered {
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
                        param_bind_rect,
                        (4.0 * scale) as i32,
                        param_bind_bg,
                    );
                    SetTextColor(dib_dc, param_bind_bg.contrasting_text_color().to_colorref());
                    SelectObject(dib_dc, small_font);
                    let mut bind_wz = to_utf16_z(if state.is_recording_param {
                        "Listening"
                    } else {
                        "Bind"
                    });
                    let mut bind_rc = param_bind_rect;
                    DrawTextW(
                        dib_dc,
                        &mut bind_wz,
                        &mut bind_rc,
                        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
                    );
                    draw_rounded_border_in_buffer(
                        bits,
                        w,
                        h,
                        param_bind_rect,
                        (4.0 * scale) as i32,
                        1,
                        theme.border,
                    );
                }

                _ => {
                    // ── Non-KeyMapping: text-field editor ──
                    let has_side_btn = param_schema == ActionParamSchema::FilePath;
                    let param_field_rect = RECT {
                        left: pad + label_w + 8,
                        top: param_label_y,
                        right: if has_side_btn {
                            w - pad - bind_btn_w - 8
                        } else {
                            w - pad
                        },
                        bottom: param_label_y + field_h,
                    };
                    let is_editing = state.is_editing_param;
                    let is_param_hovered = state.hovered_target == BindingPopupHit::ParamField
                        || state.hovered_target == BindingPopupHit::BrowseParam;
                    let param_bg = if is_param_hovered {
                        theme
                            .hover
                            .blend_over(theme.surface.blend_over(theme.background))
                    } else {
                        theme.surface.blend_over(theme.background)
                    };
                    let param_border = if is_editing {
                        theme.accent
                    } else {
                        theme.border
                    };
                    let radius = (4.0 * scale) as i32;
                    draw_rounded_rect_in_buffer(bits, w, h, param_field_rect, radius, param_bg);
                    draw_rounded_border_in_buffer(
                        bits,
                        w,
                        h,
                        param_field_rect,
                        radius,
                        1,
                        param_border,
                    );

                    let param_text = if state.param.is_empty() && !is_editing {
                        match param_schema {
                            ActionParamSchema::FilePath => "(click Browse to select)",
                            _ => "",
                        }
                    } else {
                        &state.param
                    };
                    SelectObject(dib_dc, body_font);
                    SetTextColor(dib_dc, theme.text.to_colorref());
                    let inset = (6.0 * scale) as i32;
                    let mut text_rc = RECT {
                        left: param_field_rect.left + inset,
                        top: param_field_rect.top,
                        right: param_field_rect.right - inset,
                        bottom: param_field_rect.bottom,
                    };

                    if is_editing {
                        // Draw text suitable for editing (no ellipsis so the
                        // cursor can reach the far end)
                        let mut wz = to_utf16_z(param_text);
                        let _ = DrawTextW(
                            dib_dc,
                            &mut wz,
                            &mut text_rc,
                            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
                        );

                        // Draw cursor vertical bar at the cursor position
                        let cursor_x = cursor_position_x(
                            dib_dc,
                            &state.param,
                            state.param_edit_cursor,
                            param_field_rect.left + inset,
                        );
                        if cursor_x < param_field_rect.right - 2 {
                            let cursor_h = (20.0 * scale) as i32;
                            let cy = param_field_rect.top
                                + (param_field_rect.bottom - param_field_rect.top - cursor_h) / 2;
                            let mut cursor_rc = RECT {
                                left: cursor_x,
                                top: cy,
                                right: cursor_x + ((1.0 * scale) as i32).max(1),
                                bottom: cy + cursor_h,
                            };
                            let cursor_brush = CreateSolidBrush(theme.text.to_colorref());
                            FillRect(dib_dc, &mut cursor_rc, cursor_brush);
                            let _ = DeleteObject(cursor_brush);
                        }
                    } else {
                        let mut wz = to_utf16_z(param_text);
                        let _ = DrawTextW(
                            dib_dc,
                            &mut wz,
                            &mut text_rc,
                            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
                        );
                    }

                    // Browse button for FilePath
                    if has_side_btn {
                        let btn_rect = RECT {
                            left: w - pad - bind_btn_w,
                            top: param_label_y,
                            right: w - pad,
                            bottom: param_label_y + field_h,
                        };
                        let is_browse_hovered =
                            state.hovered_target == BindingPopupHit::BrowseParam;
                        let btn_bg = if is_browse_hovered {
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
                            btn_rect,
                            (4.0 * scale) as i32,
                            btn_bg,
                        );
                        SetTextColor(dib_dc, btn_bg.contrasting_text_color().to_colorref());
                        SelectObject(dib_dc, small_font);
                        let mut btn_wz = to_utf16_z("Browse\u{2026}");
                        let mut btn_rc = btn_rect;
                        DrawTextW(
                            dib_dc,
                            &mut btn_wz,
                            &mut btn_rc,
                            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
                        );
                        draw_rounded_border_in_buffer(
                            bits,
                            w,
                            h,
                            btn_rect,
                            (4.0 * scale) as i32,
                            1,
                            theme.border,
                        );
                    }
                }
            }
        }

        // ── Description / info block ─────────────────────────────────
        if desc.param_key.is_some() || !desc.description.is_empty() {
            let desc_row_y = if desc.param_key.is_some() {
                param_label_y + row_h
            } else {
                action_label_y + row_h
            };
            let desc_y = desc_row_y + (12.0 * scale) as i32;
            let desc_h = (60.0 * scale) as i32;
            let desc_rect = RECT {
                left: pad,
                top: desc_y,
                right: w - pad,
                bottom: desc_y + desc_h,
            };

            // Background
            let desc_bg = theme.surface.blend_over(theme.background).with_alpha(80);
            draw_rounded_rect_in_buffer(bits, w, h, desc_rect, (6.0 * scale) as i32, desc_bg);

            SelectObject(dib_dc, small_font);
            let inset = (8.0 * scale) as i32;

            // Description line
            SetTextColor(dib_dc, theme.text_muted.to_colorref());
            let mut text_wz = to_utf16_z(desc.description);
            let mut text_rc = RECT {
                left: desc_rect.left + inset,
                top: desc_rect.top + (4.0 * scale) as i32,
                right: desc_rect.right - inset,
                bottom: desc_rect.top + (22.0 * scale) as i32,
            };
            let _ = DrawTextW(
                dib_dc,
                &mut text_wz,
                &mut text_rc,
                DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS,
            );

            // Schema-specific hint / error line
            let (hint_color, hint_text) = if let Some(ref err) = state.param_save_error {
                (Argb::new(255, 220, 60, 60), err.clone())
            } else {
                let text = match param_schema {
                    ActionParamSchema::Number { unit, min, max } => {
                        format!("Range: {}{} to {}{}", min, unit, max, unit)
                    }
                    ActionParamSchema::FilePath => {
                        "Click Browse to select a file or executable.".to_string()
                    }
                    ActionParamSchema::Text => {
                        if desc.name == "run_ps" {
                            "Example: Get-Process | Where-Object { $_.CPU -gt 10 }".to_string()
                        } else if desc.name == "switch_power_plan" {
                            "Enter the GUID or name of the power plan.".to_string()
                        } else {
                            String::new()
                        }
                    }
                    ActionParamSchema::KeyMapping => {
                        "Click Bind to record a key combination.".to_string()
                    }
                    ActionParamSchema::None | ActionParamSchema::PowerAction => String::new(),
                };
                (theme.text.with_alpha(180), text)
            };
            if !hint_text.is_empty() {
                SetTextColor(dib_dc, hint_color.to_colorref());
                let mut hint_wz = to_utf16_z(&hint_text);
                let mut hint_rc = RECT {
                    left: desc_rect.left + inset,
                    top: text_rc.bottom + (2.0 * scale) as i32,
                    right: desc_rect.right - inset,
                    bottom: desc_rect.bottom - (4.0 * scale) as i32,
                };
                let _ = DrawTextW(
                    dib_dc,
                    &mut hint_wz,
                    &mut hint_rc,
                    DT_LEFT | DT_WORDBREAK | DT_END_ELLIPSIS,
                );
            }
        }

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

        // ── Trigger key dropdown, drawn above rows underneath ─────────────
        if state.trigger_editor.dropdown.is_open {
            if let Some(slot) = state.trigger_editor.open_slot {
                let slot_rect = trigger_slot_rects(trigger_field_rect, scale)
                    .into_iter()
                    .find(|(candidate, _)| *candidate == slot)
                    .map(|(_, rect)| rect)
                    .unwrap_or(trigger_field_rect);
                let key_items = state.trigger_editor.items_for_open_slot();
                let item_h = (24.0 * scale) as i32;
                let search_h = (30.0 * scale) as i32;
                let visible_rows = key_dropdown_visible_rows();
                let filtered_count = state.trigger_editor.dropdown.filtered_count(&key_items);
                let visible_items = state
                    .trigger_editor
                    .dropdown
                    .visible_items(&key_items, visible_rows);
                let list_rows = visible_items.len().max(1);
                let popup_rect = RECT {
                    left: slot_rect.left,
                    top: slot_rect.bottom,
                    right: (slot_rect.left + (220.0 * scale) as i32).min(w - pad),
                    bottom: slot_rect.bottom + search_h + list_rows as i32 * item_h,
                };
                let popup_bg = theme.surface.blend_over(theme.background);
                draw_rounded_rect_in_buffer(bits, w, h, popup_rect, (4.0 * scale) as i32, popup_bg);
                draw_rounded_border_in_buffer(
                    bits,
                    w,
                    h,
                    popup_rect,
                    (4.0 * scale) as i32,
                    1,
                    theme.border,
                );

                let search_rect = RECT {
                    left: popup_rect.left + (6.0 * scale) as i32,
                    top: popup_rect.top + (5.0 * scale) as i32,
                    right: popup_rect.right - (6.0 * scale) as i32,
                    bottom: popup_rect.top + search_h - (5.0 * scale) as i32,
                };
                draw_rounded_rect_in_buffer(
                    bits,
                    w,
                    h,
                    search_rect,
                    (4.0 * scale) as i32,
                    theme.background.blend_over(popup_bg),
                );
                draw_rounded_border_in_buffer(
                    bits,
                    w,
                    h,
                    search_rect,
                    (4.0 * scale) as i32,
                    1,
                    theme.border,
                );

                SelectObject(dib_dc, small_font);
                let search_text = if state.trigger_editor.dropdown.filter.is_empty() {
                    "Search keys..."
                } else {
                    state.trigger_editor.dropdown.filter.as_str()
                };
                SetTextColor(
                    dib_dc,
                    if state.trigger_editor.dropdown.filter.is_empty() {
                        theme.text_muted
                    } else {
                        theme.text
                    }
                    .to_colorref(),
                );
                let mut search_wz = to_utf16_z(search_text);
                let mut search_text_rect = RECT {
                    left: search_rect.left + (7.0 * scale) as i32,
                    top: search_rect.top,
                    right: search_rect.right - (7.0 * scale) as i32,
                    bottom: search_rect.bottom,
                };
                DrawTextW(
                    dib_dc,
                    &mut search_wz,
                    &mut search_text_rect,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                );

                let list_top = popup_rect.top + search_h;
                for (i, item) in visible_items.iter().enumerate() {
                    let item_rect = RECT {
                        left: popup_rect.left,
                        top: list_top + i as i32 * item_h,
                        right: popup_rect.right,
                        bottom: list_top + (i as i32 + 1) * item_h,
                    };
                    let is_hovered =
                        state.hovered_target == BindingPopupHit::TriggerKeyItem(item.id);
                    if is_hovered {
                        draw_rounded_rect_in_buffer(
                            bits,
                            w,
                            h,
                            item_rect,
                            0,
                            theme.hover.blend_over(popup_bg),
                        );
                    }

                    SetTextColor(dib_dc, theme.text.to_colorref());
                    let mut item_wz = to_utf16_z(&item.label);
                    let mut item_text_rect = RECT {
                        left: item_rect.left + (8.0 * scale) as i32,
                        top: item_rect.top,
                        right: item_rect.right - (8.0 * scale) as i32,
                        bottom: item_rect.bottom,
                    };
                    DrawTextW(
                        dib_dc,
                        &mut item_wz,
                        &mut item_text_rect,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                    );
                }

                if visible_items.is_empty() {
                    let item_rect = RECT {
                        left: popup_rect.left,
                        top: list_top,
                        right: popup_rect.right,
                        bottom: list_top + item_h,
                    };
                    SetTextColor(dib_dc, theme.text_muted.to_colorref());
                    let mut item_wz = to_utf16_z("No keys found");
                    let mut item_text_rect = RECT {
                        left: item_rect.left + (8.0 * scale) as i32,
                        top: item_rect.top,
                        right: item_rect.right - (8.0 * scale) as i32,
                        bottom: item_rect.bottom,
                    };
                    DrawTextW(
                        dib_dc,
                        &mut item_wz,
                        &mut item_text_rect,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                    );
                }

                if filtered_count > visible_rows {
                    let scroll_w = (4.0 * scale).max(3.0) as i32;
                    let track_rect = RECT {
                        left: popup_rect.right - scroll_w - 2,
                        top: list_top + 2,
                        right: popup_rect.right - 2,
                        bottom: popup_rect.bottom - 2,
                    };
                    draw_rounded_rect_in_buffer(
                        bits,
                        w,
                        h,
                        track_rect,
                        scroll_w / 2,
                        theme.border.with_alpha(60),
                    );
                    let travel = (track_rect.bottom - track_rect.top).max(1);
                    let thumb_h =
                        ((visible_rows as f32 / filtered_count as f32) * travel as f32) as i32;
                    let thumb_h = thumb_h.max((16.0 * scale) as i32);
                    let max_scroll = filtered_count.saturating_sub(visible_rows).max(1);
                    let thumb_y = track_rect.top
                        + ((state.trigger_editor.dropdown.scroll as f32 / max_scroll as f32)
                            * (travel - thumb_h).max(0) as f32) as i32;
                    let thumb_rect = RECT {
                        left: track_rect.left,
                        top: thumb_y,
                        right: track_rect.right,
                        bottom: thumb_y + thumb_h,
                    };
                    draw_rounded_rect_in_buffer(bits, w, h, thumb_rect, scroll_w / 2, theme.hover);
                }
            }
        }

        // ── Action dropdown, drawn last so it sits above fields/footer ───
        if state.action_dropdown.is_open {
            let item_h = (24.0 * scale) as i32;
            let search_h = (30.0 * scale) as i32;
            let visible_rows = action_dropdown_visible_rows();
            let filtered_count = state.action_dropdown.filtered_count(&state.action_items);
            let visible_actions = state
                .action_dropdown
                .visible_items(&state.action_items, visible_rows);
            let list_rows = visible_actions.len().max(1);
            let popup_rect = RECT {
                left: combo_rect.left,
                top: combo_rect.bottom,
                right: (combo_rect.left + (260.0 * scale) as i32).min(w - pad),
                bottom: combo_rect.bottom + search_h + list_rows as i32 * item_h,
            };
            let popup_bg = theme.surface.blend_over(theme.background);
            draw_rounded_rect_in_buffer(bits, w, h, popup_rect, (4.0 * scale) as i32, popup_bg);
            draw_rounded_border_in_buffer(
                bits,
                w,
                h,
                popup_rect,
                (4.0 * scale) as i32,
                1,
                theme.border,
            );

            let search_rect = RECT {
                left: popup_rect.left + (6.0 * scale) as i32,
                top: popup_rect.top + (5.0 * scale) as i32,
                right: popup_rect.right - (6.0 * scale) as i32,
                bottom: popup_rect.top + search_h - (5.0 * scale) as i32,
            };
            draw_rounded_rect_in_buffer(
                bits,
                w,
                h,
                search_rect,
                (4.0 * scale) as i32,
                theme.background.blend_over(popup_bg),
            );
            draw_rounded_border_in_buffer(
                bits,
                w,
                h,
                search_rect,
                (4.0 * scale) as i32,
                1,
                theme.border,
            );

            SelectObject(dib_dc, small_font);
            let search_text = if state.action_dropdown.filter.is_empty() {
                "Search actions..."
            } else {
                state.action_dropdown.filter.as_str()
            };
            SetTextColor(
                dib_dc,
                if state.action_dropdown.filter.is_empty() {
                    theme.text_muted
                } else {
                    theme.text
                }
                .to_colorref(),
            );
            let mut search_wz = to_utf16_z(search_text);
            let mut search_text_rect = RECT {
                left: search_rect.left + (7.0 * scale) as i32,
                top: search_rect.top,
                right: search_rect.right - (7.0 * scale) as i32,
                bottom: search_rect.bottom,
            };
            DrawTextW(
                dib_dc,
                &mut search_wz,
                &mut search_text_rect,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            );

            let list_top = popup_rect.top + search_h;
            for (i, action) in visible_actions.iter().enumerate() {
                let item_rect = RECT {
                    left: popup_rect.left,
                    top: list_top + i as i32 * item_h,
                    right: popup_rect.right,
                    bottom: list_top + (i as i32 + 1) * item_h,
                };
                let is_hovered = state.hovered_target == BindingPopupHit::KindItem(action.id);
                let is_selected = state.kind_idx == action.id;
                if is_hovered || is_selected {
                    let bg = if is_hovered {
                        theme.hover.blend_over(popup_bg)
                    } else {
                        theme.selected.blend_over(popup_bg)
                    };
                    draw_rounded_rect_in_buffer(bits, w, h, item_rect, 0, bg);
                }

                SetTextColor(dib_dc, theme.text.to_colorref());
                let mut item_wz = to_utf16_z(&action.label);
                let mut item_text_rect = RECT {
                    left: item_rect.left + (8.0 * scale) as i32,
                    top: item_rect.top,
                    right: item_rect.right - (8.0 * scale) as i32,
                    bottom: item_rect.bottom,
                };
                DrawTextW(
                    dib_dc,
                    &mut item_wz,
                    &mut item_text_rect,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                );
            }

            if visible_actions.is_empty() {
                let item_rect = RECT {
                    left: popup_rect.left,
                    top: list_top,
                    right: popup_rect.right,
                    bottom: list_top + item_h,
                };
                SetTextColor(dib_dc, theme.text_muted.to_colorref());
                let mut item_wz = to_utf16_z("No actions found");
                let mut item_text_rect = RECT {
                    left: item_rect.left + (8.0 * scale) as i32,
                    top: item_rect.top,
                    right: item_rect.right - (8.0 * scale) as i32,
                    bottom: item_rect.bottom,
                };
                DrawTextW(
                    dib_dc,
                    &mut item_wz,
                    &mut item_text_rect,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                );
            }

            if filtered_count > visible_rows {
                let scroll_w = (4.0 * scale).max(3.0) as i32;
                let track_rect = RECT {
                    left: popup_rect.right - scroll_w - 2,
                    top: list_top + 2,
                    right: popup_rect.right - 2,
                    bottom: popup_rect.bottom - 2,
                };
                draw_rounded_rect_in_buffer(
                    bits,
                    w,
                    h,
                    track_rect,
                    scroll_w / 2,
                    theme.border.with_alpha(60),
                );
                let travel = (track_rect.bottom - track_rect.top).max(1);
                let thumb_h =
                    ((visible_rows as f32 / filtered_count as f32) * travel as f32) as i32;
                let thumb_h = thumb_h.max((16.0 * scale) as i32);
                let max_scroll = filtered_count.saturating_sub(visible_rows).max(1);
                let thumb_y = track_rect.top
                    + ((state.action_dropdown.scroll as f32 / max_scroll as f32)
                        * (travel - thumb_h).max(0) as f32) as i32;
                let thumb_rect = RECT {
                    left: track_rect.left,
                    top: thumb_y,
                    right: track_rect.right,
                    bottom: thumb_y + thumb_h,
                };
                draw_rounded_rect_in_buffer(bits, w, h, thumb_rect, scroll_w / 2, theme.hover);
            }

            let described_id = match state.hovered_target {
                BindingPopupHit::KindItem(id) => id,
                _ => state.kind_idx,
            };
            if let Some(action) = state
                .action_items
                .iter()
                .find(|item| item.id == described_id)
                .filter(|item| {
                    item.description
                        .as_deref()
                        .is_some_and(|text| !text.is_empty())
                })
            {
                let gap = (8.0 * scale) as i32;
                let desc_rect = RECT {
                    left: popup_rect.right + gap,
                    top: popup_rect.top,
                    right: (popup_rect.right + gap + (220.0 * scale) as i32).min(w - pad),
                    bottom: popup_rect.bottom,
                };
                if desc_rect.right > desc_rect.left + (80.0 * scale) as i32 {
                    draw_rounded_rect_in_buffer(
                        bits,
                        w,
                        h,
                        desc_rect,
                        (4.0 * scale) as i32,
                        popup_bg,
                    );
                    draw_rounded_border_in_buffer(
                        bits,
                        w,
                        h,
                        desc_rect,
                        (4.0 * scale) as i32,
                        1,
                        theme.border,
                    );

                    let inset = (10.0 * scale) as i32;
                    SelectObject(dib_dc, small_font);
                    SetTextColor(dib_dc, theme.text.to_colorref());
                    let mut title_wz = to_utf16_z(&action.label);
                    let mut title_rect = RECT {
                        left: desc_rect.left + inset,
                        top: desc_rect.top + inset,
                        right: desc_rect.right - inset,
                        bottom: desc_rect.top + inset + (22.0 * scale) as i32,
                    };
                    DrawTextW(
                        dib_dc,
                        &mut title_wz,
                        &mut title_rect,
                        DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS,
                    );

                    SetTextColor(dib_dc, theme.text_muted.to_colorref());
                    let mut desc_wz = to_utf16_z(action.description.as_deref().unwrap_or(""));
                    let mut text_rect = RECT {
                        left: desc_rect.left + inset,
                        top: title_rect.bottom + (4.0 * scale) as i32,
                        right: desc_rect.right - inset,
                        bottom: desc_rect.bottom - inset,
                    };
                    DrawTextW(
                        dib_dc,
                        &mut desc_wz,
                        &mut text_rect,
                        DT_LEFT | DT_WORDBREAK | DT_END_ELLIPSIS,
                    );
                }
            }
        }

        // ── Param editor key dropdown (for KeyMapping schema) ───────────
        if desc.param_key.is_some()
            && param_schema == ActionParamSchema::KeyMapping
            && state.param_editor.dropdown.is_open
            && let Some(slot) = state.param_editor.open_slot
        {
            let param_label_y = action_label_y + row_h + (4.0 * scale) as i32;
            let param_key_field_rect = RECT {
                left: pad + label_w + 8,
                top: param_label_y,
                right: w - pad - bind_btn_w - 8,
                bottom: param_label_y + field_h,
            };
            let slot_rect = {
                let rects = trigger_slot_rects(param_key_field_rect, scale);
                rects
                    .into_iter()
                    .find(|(candidate, _)| *candidate == slot)
                    .map(|(_, rect)| rect)
                    .unwrap_or(param_key_field_rect)
            };
            let key_items = state.param_editor.items_for_open_slot();
            let item_h = (24.0 * scale) as i32;
            let search_h = (30.0 * scale) as i32;
            let visible_rows = key_dropdown_visible_rows();
            let filtered_count = state.param_editor.dropdown.filtered_count(&key_items);
            let visible_items = state
                .param_editor
                .dropdown
                .visible_items(&key_items, visible_rows);
            let list_rows = visible_items.len().max(1);
            let popup_rect = RECT {
                left: slot_rect.left,
                top: slot_rect.bottom,
                right: (slot_rect.left + (220.0 * scale) as i32).min(w - pad),
                bottom: slot_rect.bottom + search_h + list_rows as i32 * item_h,
            };
            let popup_bg = theme.surface.blend_over(theme.background);
            draw_rounded_rect_in_buffer(bits, w, h, popup_rect, (4.0 * scale) as i32, popup_bg);
            draw_rounded_border_in_buffer(
                bits,
                w,
                h,
                popup_rect,
                (4.0 * scale) as i32,
                1,
                theme.border,
            );

            let search_rect = RECT {
                left: popup_rect.left + (6.0 * scale) as i32,
                top: popup_rect.top + (5.0 * scale) as i32,
                right: popup_rect.right - (6.0 * scale) as i32,
                bottom: popup_rect.top + search_h - (5.0 * scale) as i32,
            };
            draw_rounded_rect_in_buffer(
                bits,
                w,
                h,
                search_rect,
                (4.0 * scale) as i32,
                theme.background.blend_over(popup_bg),
            );
            draw_rounded_border_in_buffer(
                bits,
                w,
                h,
                search_rect,
                (4.0 * scale) as i32,
                1,
                theme.border,
            );

            SelectObject(dib_dc, small_font);
            let search_text = if state.param_editor.dropdown.filter.is_empty() {
                "Search keys..."
            } else {
                state.param_editor.dropdown.filter.as_str()
            };
            SetTextColor(
                dib_dc,
                if state.param_editor.dropdown.filter.is_empty() {
                    theme.text_muted
                } else {
                    theme.text
                }
                .to_colorref(),
            );
            let mut search_wz = to_utf16_z(search_text);
            let mut search_text_rect = RECT {
                left: search_rect.left + (7.0 * scale) as i32,
                top: search_rect.top,
                right: search_rect.right - (7.0 * scale) as i32,
                bottom: search_rect.bottom,
            };
            DrawTextW(
                dib_dc,
                &mut search_wz,
                &mut search_text_rect,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            );

            let list_top = popup_rect.top + search_h;
            for (i, item) in visible_items.iter().enumerate() {
                let item_rect = RECT {
                    left: popup_rect.left,
                    top: list_top + i as i32 * item_h,
                    right: popup_rect.right,
                    bottom: list_top + (i as i32 + 1) * item_h,
                };
                let is_hovered = state.hovered_target == BindingPopupHit::ParamKeyItem(item.id);
                if is_hovered {
                    draw_rounded_rect_in_buffer(
                        bits,
                        w,
                        h,
                        item_rect,
                        0,
                        theme.hover.blend_over(popup_bg),
                    );
                }

                SetTextColor(dib_dc, theme.text.to_colorref());
                let mut item_wz = to_utf16_z(&item.label);
                let mut item_text_rect = RECT {
                    left: item_rect.left + (8.0 * scale) as i32,
                    top: item_rect.top,
                    right: item_rect.right - (8.0 * scale) as i32,
                    bottom: item_rect.bottom,
                };
                DrawTextW(
                    dib_dc,
                    &mut item_wz,
                    &mut item_text_rect,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                );
            }

            if visible_items.is_empty() {
                let item_rect = RECT {
                    left: popup_rect.left,
                    top: list_top,
                    right: popup_rect.right,
                    bottom: list_top + item_h,
                };
                SetTextColor(dib_dc, theme.text_muted.to_colorref());
                let mut item_wz = to_utf16_z("No keys found");
                let mut item_text_rect = RECT {
                    left: item_rect.left + (8.0 * scale) as i32,
                    top: item_rect.top,
                    right: item_rect.right - (8.0 * scale) as i32,
                    bottom: item_rect.bottom,
                };
                DrawTextW(
                    dib_dc,
                    &mut item_wz,
                    &mut item_text_rect,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                );
            }

            if filtered_count > visible_rows {
                let scroll_w = (4.0 * scale).max(3.0) as i32;
                let track_rect = RECT {
                    left: popup_rect.right - scroll_w - 2,
                    top: list_top + 2,
                    right: popup_rect.right - 2,
                    bottom: popup_rect.bottom - 2,
                };
                draw_rounded_rect_in_buffer(
                    bits,
                    w,
                    h,
                    track_rect,
                    scroll_w / 2,
                    theme.border.with_alpha(60),
                );
                let travel = (track_rect.bottom - track_rect.top).max(1);
                let thumb_h =
                    ((visible_rows as f32 / filtered_count as f32) * travel as f32) as i32;
                let thumb_h = thumb_h.max((16.0 * scale) as i32);
                let max_scroll = filtered_count.saturating_sub(visible_rows).max(1);
                let thumb_y = track_rect.top
                    + ((state.param_editor.dropdown.scroll as f32 / max_scroll as f32)
                        * (travel - thumb_h).max(0) as f32) as i32;
                let thumb_rect = RECT {
                    left: track_rect.left,
                    top: thumb_y,
                    right: track_rect.right,
                    bottom: thumb_y + thumb_h,
                };
                draw_rounded_rect_in_buffer(bits, w, h, thumb_rect, scroll_w / 2, theme.hover);
            }
        }

        // ── Cleanup ────────────────────────────────────────────────
        let _ = DeleteObject(sep_brush);
        SelectObject(dib_dc, old_font);
        let _ = DeleteObject(title_font);
        let _ = DeleteObject(body_font);
        let _ = DeleteObject(small_font);

        frame.fix_gdi_alpha(popup_bg);

        let cur_pos = {
            let mut wr = RECT::default();
            let _ = GetWindowRect(hwnd, &mut wr);
            (wr.left, wr.top)
        };
        frame.present_layered(hwnd, cur_pos.0, cur_pos.1, 255);
    }
}

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

fn action_dropdown_visible_rows() -> usize {
    8
}

fn key_dropdown_visible_rows() -> usize {
    8
}

/// Return the default param value for a given schema.
///
/// Called when the user switches the action kind, so stale data from the
/// previous action is replaced with something sensible for the new one.
fn default_param_for_schema(schema: ActionParamSchema) -> String {
    match schema {
        ActionParamSchema::Number { .. } => "5".to_string(),
        ActionParamSchema::None
        | ActionParamSchema::PowerAction
        | ActionParamSchema::Text
        | ActionParamSchema::FilePath
        | ActionParamSchema::KeyMapping => String::new(),
    }
}

/// Open a file picker dialog for FilePath action parameters.
fn pick_param_file(parent: HWND) -> Option<String> {
    use std::mem;
    unsafe {
        let mut ofn: windows::Win32::UI::Controls::Dialogs::OPENFILENAMEW = mem::zeroed();
        let mut buf = [0u16; 1024];
        let filter: Vec<u16> = "All Files\0*.*\0".encode_utf16().collect();

        ofn.lStructSize =
            mem::size_of::<windows::Win32::UI::Controls::Dialogs::OPENFILENAMEW>() as u32;
        ofn.hwndOwner = parent;
        ofn.lpstrFilter = windows::core::PCWSTR::from_raw(filter.as_ptr());
        ofn.lpstrFile = windows::core::PWSTR(buf.as_mut_ptr());
        ofn.nMaxFile = buf.len() as u32;
        ofn.lpstrTitle = windows::core::w!("Select File");
        ofn.Flags = windows::Win32::UI::Controls::Dialogs::OFN_FILEMUSTEXIST
            | windows::Win32::UI::Controls::Dialogs::OFN_HIDEREADONLY
            | windows::Win32::UI::Controls::Dialogs::OFN_PATHMUSTEXIST;

        if windows::Win32::UI::Controls::Dialogs::GetOpenFileNameW(&mut ofn).as_bool() {
            let len = (0..buf.len()).find(|&i| buf[i] == 0).unwrap_or(0);
            if len > 0 {
                return Some(String::from_utf16_lossy(&buf[..len]));
            }
        }
    }
    None
}

fn open_kind_dropdown(state: &mut BindingPopupState) {
    state.trigger_editor.close_dropdown();
    state.action_dropdown.open(
        &state.action_items,
        state.kind_idx,
        action_dropdown_visible_rows(),
    );
}

fn trigger_slot_rects(field_rect: RECT, scale: f32) -> Vec<(KeyComboSlot, RECT)> {
    let gap = (6.0 * scale) as i32;
    let plus_w = (10.0 * scale) as i32;
    let total_plus = plus_w * 3;
    let total_gap = gap * 6;
    let slot_w = ((field_rect.right - field_rect.left - total_plus - total_gap) / 4).max(32);
    let mut x = field_rect.left;
    let mut rects = Vec::with_capacity(4);
    for i in 0..4 {
        let slot = if i < 3 {
            KeyComboSlot::Modifier(i)
        } else {
            KeyComboSlot::Key
        };
        let left = x;
        let right = if i == 3 {
            field_rect.right
        } else {
            left + slot_w
        };
        rects.push((
            slot,
            RECT {
                left,
                top: field_rect.top,
                right,
                bottom: field_rect.bottom,
            },
        ));
        x = right + plus_w + gap * 2;
    }
    rects
}

/// Commit any in-progress param text edit, clearing the editing state.
/// Validates number ranges and clamps them on commit.
fn commit_param_edit(state: &mut BindingPopupState) {
    state.is_editing_param = false;
    state.param_edit_cursor = state.param.len();
    state.param_save_error = None;

    let schema = crate::config::editor_layout::editor_action_desc(state.kind_idx).param_schema;
    if let ActionParamSchema::Number { min, max, .. } = schema {
        let val: i32 = state.param.trim().parse().unwrap_or(min);
        if val < min || val > max {
            if val < min {
                state.param = min.to_string();
            } else {
                state.param = max.to_string();
            }
        } else {
            state.param = val.to_string();
        }
    }
}

/// Cancel any in-progress param text edit, restoring the old value.
fn cancel_param_edit(state: &mut BindingPopupState) {
    if state.is_editing_param {
        state.param = std::mem::take(&mut state.param_edit_old);
        state.is_editing_param = false;
        state.param_edit_cursor = state.param.len();
    }
    state.param_save_error = None;
}

/// Compute the X coordinate of the cursor (insertion point) within the param
/// edit field, using the current font to measure text before the cursor.
fn cursor_position_x(dib_dc: HDC, text: &str, cursor: usize, field_left: i32) -> i32 {
    if cursor == 0 || text.is_empty() || cursor > text.len() {
        return field_left;
    }
    // cursor is a byte index; find the nearest char boundary just in case
    let bound = text.floor_char_boundary(cursor);
    let prefix = &text[..bound];
    let wz: Vec<u16> = prefix.encode_utf16().collect();
    unsafe {
        let mut sz = windows::Win32::Foundation::SIZE::default();
        let _ = GetTextExtentPoint32W(dib_dc, &wz, &mut sz);
        field_left + sz.cx
    }
}

// ── Window procedure ─────────────────────────────────────────────────

unsafe extern "system" fn binding_popup_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
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

                // If editing param and clicking something else, commit the edit
                if state.is_editing_param && hit != BindingPopupHit::ParamField {
                    commit_param_edit(state);
                }

                match hit {
                    BindingPopupHit::TriggerSlot(slot) => {
                        state.action_dropdown.close();
                        state.param_editor.close_dropdown();
                        if state.trigger_editor.open_slot == Some(slot)
                            && state.trigger_editor.dropdown.is_open
                        {
                            state.trigger_editor.close_dropdown();
                        } else {
                            state
                                .trigger_editor
                                .open_dropdown(slot, key_dropdown_visible_rows());
                        }
                        paint_binding_popup(hwnd, state_ptr);
                    }
                    BindingPopupHit::ParamSlot(slot) => {
                        state.action_dropdown.close();
                        state.trigger_editor.close_dropdown();
                        if state.param_editor.open_slot == Some(slot)
                            && state.param_editor.dropdown.is_open
                        {
                            state.param_editor.close_dropdown();
                        } else {
                            state
                                .param_editor
                                .open_dropdown(slot, key_dropdown_visible_rows());
                        }
                        paint_binding_popup(hwnd, state_ptr);
                    }
                    BindingPopupHit::RecordTrigger => {
                        state.param_editor.close_dropdown();
                        state.is_recording_trigger = !state.is_recording_trigger;
                        state.is_recording_param = false;
                        if state.is_recording_trigger {
                            // Send WM_BINDING_CAPTURED to THIS popup, not the parent
                            crate::hook::set_recording_window(Some(hwnd));
                        } else {
                            crate::hook::set_recording_window(None);
                        }
                        paint_binding_popup(hwnd, state_ptr);
                    }
                    BindingPopupHit::KindCombo => {
                        state.trigger_editor.close_dropdown();
                        if state.action_dropdown.is_open {
                            state.action_dropdown.close();
                        } else {
                            open_kind_dropdown(state);
                        }
                        paint_binding_popup(hwnd, state_ptr);
                    }
                    BindingPopupHit::RecordParam => {
                        state.is_recording_param = !state.is_recording_param;
                        state.is_recording_trigger = false;
                        if state.is_recording_param {
                            // Send WM_BINDING_CAPTURED to THIS popup, not the parent
                            crate::hook::set_recording_window(Some(hwnd));
                        } else {
                            crate::hook::set_recording_window(None);
                        }
                        paint_binding_popup(hwnd, state_ptr);
                    }
                    BindingPopupHit::BrowseParam => {
                        // Open file picker for FilePath actions
                        state.is_recording_trigger = false;
                        state.is_recording_param = false;
                        crate::hook::set_recording_window(None);
                        if let Some(path) = pick_param_file(hwnd) {
                            state.param = path;
                        }
                        paint_binding_popup(hwnd, state_ptr);
                    }
                    BindingPopupHit::ParamField => {
                        let schema = editor_action_desc(state.kind_idx).param_schema;
                        if matches!(
                            schema,
                            ActionParamSchema::Text | ActionParamSchema::Number { .. }
                        ) {
                            if !state.is_editing_param {
                                state.is_editing_param = true;
                                state.param_edit_old = state.param.clone();
                            }
                            state.param_edit_cursor = state.param.len();
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
                        state.saved = true;
                        state.should_close = true;
                    }
                    BindingPopupHit::CancelBtn => {
                        crate::hook::set_recording_window(None);
                        state.should_close = true;
                    }
                    BindingPopupHit::KindItem(idx) => {
                        if state.kind_idx == idx {
                            // Same action — just close dropdown, no reset needed
                            state.action_dropdown.close();
                            paint_binding_popup(hwnd, state_ptr);
                        } else {
                            state.kind_idx = idx;
                            // Reset param to a sensible default for the new schema
                            state.param =
                                default_param_for_schema(editor_action_desc(idx).param_schema);
                            // Reset param_editor to match the new param value
                            state.param_editor =
                                KeyComboEditorState::from_trigger_string(&state.param);
                            state.param_editor.close_dropdown();
                            state.is_recording_param = false;
                            crate::hook::set_recording_window(None);
                            state.action_dropdown.close();
                            state.param_save_error = None;
                            paint_binding_popup(hwnd, state_ptr);
                        }
                    }
                    BindingPopupHit::TriggerKeyItem(idx) => {
                        state.trigger_editor.choose(idx);
                        state.trigger = state.trigger_editor.to_trigger_string();
                        paint_binding_popup(hwnd, state_ptr);
                    }
                    BindingPopupHit::ParamKeyItem(idx) => {
                        state.param_editor.choose(idx);
                        state.param = state.param_editor.to_trigger_string();
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

            WM_MOUSEWHEEL => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut BindingPopupState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    if state.param_editor.dropdown.is_open {
                        let delta = (wparam.0 as i32 >> 16) as i16;
                        let items = state.param_editor.items_for_open_slot();
                        state.param_editor.dropdown.scroll_by(
                            if delta < 0 { 1 } else { -1 },
                            &items,
                            key_dropdown_visible_rows(),
                        );
                        paint_binding_popup(hwnd, state_ptr);
                    } else if state.trigger_editor.dropdown.is_open {
                        let delta = (wparam.0 as i32 >> 16) as i16;
                        let items = state.trigger_editor.items_for_open_slot();
                        state.trigger_editor.dropdown.scroll_by(
                            if delta < 0 { 1 } else { -1 },
                            &items,
                            key_dropdown_visible_rows(),
                        );
                        paint_binding_popup(hwnd, state_ptr);
                    } else if state.action_dropdown.is_open {
                        let delta = (wparam.0 as i32 >> 16) as i16;
                        state.action_dropdown.scroll_by(
                            if delta < 0 { 1 } else { -1 },
                            &state.action_items,
                            action_dropdown_visible_rows(),
                        );
                        paint_binding_popup(hwnd, state_ptr);
                    }
                }
                LRESULT(0)
            }

            WM_KEYDOWN => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut BindingPopupState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;

                    // Param text editing keys (only when editing a Text/Number field)
                    if state.is_editing_param {
                        match wparam.0 as u32 {
                            0x08 => {
                                // Backspace
                                if state.param_edit_cursor > 0 {
                                    let before = state.param_edit_cursor - 1;
                                    let after = state.param.len() - state.param_edit_cursor;
                                    let mut s = String::with_capacity(state.param.len() - 1);
                                    s.push_str(&state.param[..before]);
                                    if after > 0 {
                                        s.push_str(&state.param[state.param_edit_cursor..]);
                                    }
                                    state.param = s;
                                    state.param_edit_cursor -= 1;
                                }
                                paint_binding_popup(hwnd, state_ptr);
                            }
                            0x2E => {
                                // Delete
                                if state.param_edit_cursor < state.param.len() {
                                    let after = state.param.len() - state.param_edit_cursor - 1;
                                    let mut s = String::with_capacity(state.param.len() - 1);
                                    s.push_str(&state.param[..state.param_edit_cursor]);
                                    if after > 0 {
                                        s.push_str(&state.param[state.param_edit_cursor + 1..]);
                                    }
                                    state.param = s;
                                }
                                paint_binding_popup(hwnd, state_ptr);
                            }
                            0x25 => {
                                // Left arrow
                                if state.param_edit_cursor > 0 {
                                    state.param_edit_cursor -= 1;
                                }
                                paint_binding_popup(hwnd, state_ptr);
                            }
                            0x27 => {
                                // Right arrow
                                if state.param_edit_cursor < state.param.len() {
                                    state.param_edit_cursor += 1;
                                }
                                paint_binding_popup(hwnd, state_ptr);
                            }
                            0x0D => {
                                // Enter — commit
                                commit_param_edit(state);
                                paint_binding_popup(hwnd, state_ptr);
                            }
                            0x1B => {
                                // Escape — cancel
                                cancel_param_edit(state);
                                paint_binding_popup(hwnd, state_ptr);
                            }
                            _ => {}
                        }
                    } else if state.param_editor.dropdown.is_open {
                        let items = state.param_editor.items_for_open_slot();
                        match wparam.0 as u32 {
                            0x08 => {
                                state
                                    .param_editor
                                    .dropdown
                                    .backspace(&items, key_dropdown_visible_rows());
                                paint_binding_popup(hwnd, state_ptr);
                            }
                            0x1B => {
                                state.param_editor.close_dropdown();
                                paint_binding_popup(hwnd, state_ptr);
                            }
                            _ => {}
                        }
                    } else if state.trigger_editor.dropdown.is_open {
                        let items = state.trigger_editor.items_for_open_slot();
                        match wparam.0 as u32 {
                            0x08 => {
                                state
                                    .trigger_editor
                                    .dropdown
                                    .backspace(&items, key_dropdown_visible_rows());
                                paint_binding_popup(hwnd, state_ptr);
                            }
                            0x1B => {
                                state.trigger_editor.close_dropdown();
                                paint_binding_popup(hwnd, state_ptr);
                            }
                            _ => {}
                        }
                    } else if state.action_dropdown.is_open {
                        match wparam.0 as u32 {
                            0x08 => {
                                state
                                    .action_dropdown
                                    .backspace(&state.action_items, action_dropdown_visible_rows());
                                paint_binding_popup(hwnd, state_ptr);
                            }
                            0x1B => {
                                state.action_dropdown.close();
                                paint_binding_popup(hwnd, state_ptr);
                            }
                            _ => {}
                        }
                    }
                }
                LRESULT(0)
            }

            WM_CHAR => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut BindingPopupState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;

                    // Param text editing — insert character
                    if state.is_editing_param {
                        let ch = (wparam.0 as u32) as u8 as char;
                        let schema = editor_action_desc(state.kind_idx).param_schema;

                        // Per-schema input filtering
                        let allow = match schema {
                            ActionParamSchema::Number { .. } => {
                                // Allow digits, minus only at cursor 0 (start)
                                ch.is_ascii_digit()
                                    || (ch == '-'
                                        && state.param_edit_cursor == 0
                                        && !state.param.starts_with('-'))
                            }
                            _ => {
                                // Text / FilePath / etc: all printable chars
                                ch.is_ascii_graphic() || ch == ' '
                            }
                        };

                        if allow {
                            // Clear any previous save error when user starts typing
                            state.param_save_error = None;
                            let mut s = String::with_capacity(state.param.len() + 1);
                            s.push_str(&state.param[..state.param_edit_cursor]);
                            s.push(ch);
                            s.push_str(&state.param[state.param_edit_cursor..]);
                            state.param = s;
                            state.param_edit_cursor += 1;
                        }
                        paint_binding_popup(hwnd, state_ptr);
                    } else if state.param_editor.dropdown.is_open {
                        let ch = (wparam.0 as u32) as u8 as char;
                        let items = state.param_editor.items_for_open_slot();
                        state.param_editor.dropdown.input_char(
                            ch,
                            &items,
                            key_dropdown_visible_rows(),
                        );
                        paint_binding_popup(hwnd, state_ptr);
                    } else if state.trigger_editor.dropdown.is_open {
                        let ch = (wparam.0 as u32) as u8 as char;
                        let items = state.trigger_editor.items_for_open_slot();
                        state.trigger_editor.dropdown.input_char(
                            ch,
                            &items,
                            key_dropdown_visible_rows(),
                        );
                        paint_binding_popup(hwnd, state_ptr);
                    } else if state.action_dropdown.is_open {
                        let ch = (wparam.0 as u32) as u8 as char;
                        state.action_dropdown.input_char(
                            ch,
                            &state.action_items,
                            action_dropdown_visible_rows(),
                        );
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
                        state.trigger_editor.set_from_capture(&state.trigger);
                        state.is_recording_trigger = false;
                        crate::hook::set_recording_window(None);
                        paint_binding_popup(hwnd, state_ptr);
                    } else if state.is_recording_param {
                        let is_keymapping = editor_action_desc(state.kind_idx).param_schema
                            == ActionParamSchema::KeyMapping;
                        if is_keymapping {
                            state.param_editor.set_from_capture(&key_str);
                            state.param = state.param_editor.to_trigger_string();
                        } else {
                            state.param = key_str;
                        }
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
    let bind_btn_w = (POPUP_BIND_BUTTON_WIDTH as f32 * scale) as i32;
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

    // Open dropdown overlays get first chance at the hit-test
    // before any content below them.
    if state.action_dropdown.is_open
        && let Some(idx) = hover_kind_item(state, x, y)
    {
        return BindingPopupHit::KindItem(idx);
    }
    if state.trigger_editor.dropdown.is_open
        && let Some(idx) = hover_trigger_key_item(state, x, y)
    {
        return BindingPopupHit::TriggerKeyItem(idx);
    }
    if state.param_editor.dropdown.is_open
        && let Some(idx) = hover_param_key_item(state, x, y)
    {
        return BindingPopupHit::ParamKeyItem(idx);
    }

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

    // Record trigger button
    let rec_btn_rect = RECT {
        left: w - pad - bind_btn_w,
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

    // Trigger slots
    let trigger_field_rect = RECT {
        left: field_x,
        top: trigger_label_y,
        right: w - pad - bind_btn_w - 8,
        bottom: trigger_label_y + field_h,
    };
    for (slot, rect) in trigger_slot_rects(trigger_field_rect, scale) {
        if x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom {
            return BindingPopupHit::TriggerSlot(slot);
        }
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

    // Parameter section — only for actions with a parameter
    let desc = editor_action_desc(state.kind_idx);
    if desc.param_key.is_some() {
        let param_schema = desc.param_schema;

        match param_schema {
            ActionParamSchema::KeyMapping => {
                // KeyMapping: slot-based editor (like Trigger)
                // Side button (Bind)
                let bind_btn_rect = RECT {
                    left: w - pad - bind_btn_w,
                    top: param_label_y,
                    right: w - pad,
                    bottom: param_label_y + field_h,
                };
                if x >= bind_btn_rect.left
                    && x < bind_btn_rect.right
                    && y >= bind_btn_rect.top
                    && y < bind_btn_rect.bottom
                {
                    return BindingPopupHit::RecordParam;
                }

                // Slot rects
                let param_key_field_rect = RECT {
                    left: field_x,
                    top: param_label_y,
                    right: w - pad - bind_btn_w - 8,
                    bottom: param_label_y + field_h,
                };
                for (slot, rect) in trigger_slot_rects(param_key_field_rect, scale) {
                    if x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom {
                        return BindingPopupHit::ParamSlot(slot);
                    }
                }
            }
            _ => {
                // Non-KeyMapping: text-field editor
                let has_side_btn = param_schema == ActionParamSchema::FilePath;

                // Side button (Browse)
                if has_side_btn {
                    let browse_btn_rect = RECT {
                        left: w - pad - bind_btn_w,
                        top: param_label_y,
                        right: w - pad,
                        bottom: param_label_y + field_h,
                    };
                    if x >= browse_btn_rect.left
                        && x < browse_btn_rect.right
                        && y >= browse_btn_rect.top
                        && y < browse_btn_rect.bottom
                    {
                        return BindingPopupHit::BrowseParam;
                    }
                }

                // Param field
                let param_field_rect = RECT {
                    left: field_x,
                    top: param_label_y,
                    right: if has_side_btn {
                        w - pad - bind_btn_w - 8
                    } else {
                        w - pad
                    },
                    bottom: param_label_y + field_h,
                };
                if x >= param_field_rect.left
                    && x < param_field_rect.right
                    && y >= param_field_rect.top
                    && y < param_field_rect.bottom
                {
                    return BindingPopupHit::ParamField;
                }
            }
        }
    }

    BindingPopupHit::None
}

fn hover_kind_item(state: &BindingPopupState, x: i32, y: i32) -> Option<usize> {
    if !state.action_dropdown.is_open {
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
    let action_label_y = trigger_label_y + row_h + (4.0 * scale) as i32;
    let combo_rect = RECT {
        left: field_x,
        top: action_label_y,
        right: field_x + combo_w,
        bottom: action_label_y + field_h,
    };

    // Popup list below combo search field.
    let item_h = (24.0 * scale) as i32;
    let search_h = (30.0 * scale) as i32;
    let visible_actions = state
        .action_dropdown
        .visible_items(&state.action_items, action_dropdown_visible_rows());
    let list_rows = visible_actions.len().max(1);
    let popup_rect = RECT {
        left: combo_rect.left,
        top: combo_rect.bottom,
        right: (combo_rect.left + (260.0 * scale) as i32).min(state.win_w - pad),
        bottom: combo_rect.bottom + search_h + list_rows as i32 * item_h,
    };
    if x >= popup_rect.left && x < popup_rect.right && y >= popup_rect.top && y < popup_rect.bottom
    {
        if y < popup_rect.top + search_h {
            return None;
        }
        let row = ((y - popup_rect.top - search_h) / item_h) as usize;
        if let Some(item) = visible_actions.get(row) {
            return Some(item.id);
        }
    }
    None
}

fn hover_trigger_key_item(state: &BindingPopupState, x: i32, y: i32) -> Option<usize> {
    if !state.trigger_editor.dropdown.is_open {
        return None;
    }
    let scale = state.scale;
    let pad = (POPUP_PADDING as f32 * scale) as i32;
    let field_h = (POPUP_FIELD_HEIGHT as f32 * scale) as i32;
    let bind_btn_w = (POPUP_BIND_BUTTON_WIDTH as f32 * scale) as i32;
    let header_h = (POPUP_HEADER_HEIGHT_BASE as f32 * scale) as i32;
    let content_y = header_h + (8.0 * scale) as i32;
    let label_w = (POPUP_LABEL_WIDTH as f32 * scale) as i32;
    let field_x = pad + label_w + 8;
    let trigger_label_y = content_y;
    let trigger_field_rect = RECT {
        left: field_x,
        top: trigger_label_y,
        right: state.win_w - pad - bind_btn_w - 8,
        bottom: trigger_label_y + field_h,
    };

    let Some(slot) = state.trigger_editor.open_slot else {
        return None;
    };
    let slot_rect = trigger_slot_rects(trigger_field_rect, scale)
        .into_iter()
        .find(|(candidate, _)| *candidate == slot)
        .map(|(_, rect)| rect)
        .unwrap_or(trigger_field_rect);

    let item_h = (24.0 * scale) as i32;
    let search_h = (30.0 * scale) as i32;
    let key_items = state.trigger_editor.items_for_open_slot();
    let visible_items = state
        .trigger_editor
        .dropdown
        .visible_items(&key_items, key_dropdown_visible_rows());
    let list_rows = visible_items.len().max(1);
    let popup_rect = RECT {
        left: slot_rect.left,
        top: slot_rect.bottom,
        right: (slot_rect.left + (220.0 * scale) as i32).min(state.win_w - pad),
        bottom: slot_rect.bottom + search_h + list_rows as i32 * item_h,
    };
    if x >= popup_rect.left && x < popup_rect.right && y >= popup_rect.top && y < popup_rect.bottom
    {
        if y < popup_rect.top + search_h {
            return None;
        }
        let row = ((y - popup_rect.top - search_h) / item_h) as usize;
        if let Some(item) = visible_items.get(row) {
            return Some(item.id);
        }
    }
    None
}

fn hover_param_key_item(state: &BindingPopupState, x: i32, y: i32) -> Option<usize> {
    if !state.param_editor.dropdown.is_open {
        return None;
    }
    let scale = state.scale;
    let pad = (POPUP_PADDING as f32 * scale) as i32;
    let field_h = (POPUP_FIELD_HEIGHT as f32 * scale) as i32;
    let bind_btn_w = (POPUP_BIND_BUTTON_WIDTH as f32 * scale) as i32;
    let header_h = (POPUP_HEADER_HEIGHT_BASE as f32 * scale) as i32;
    let row_h = (POPUP_ROW_HEIGHT as f32 * scale) as i32;
    let content_y = header_h + (8.0 * scale) as i32;
    let label_w = (POPUP_LABEL_WIDTH as f32 * scale) as i32;
    let field_x = pad + label_w + 8;
    let trigger_label_y = content_y;
    let action_label_y = trigger_label_y + row_h + (4.0 * scale) as i32;
    let param_label_y = action_label_y + row_h + (4.0 * scale) as i32;
    let param_key_field_rect = RECT {
        left: field_x,
        top: param_label_y,
        right: state.win_w - pad - bind_btn_w - 8,
        bottom: param_label_y + field_h,
    };

    let Some(slot) = state.param_editor.open_slot else {
        return None;
    };
    let slot_rect = trigger_slot_rects(param_key_field_rect, scale)
        .into_iter()
        .find(|(candidate, _)| *candidate == slot)
        .map(|(_, rect)| rect)
        .unwrap_or(param_key_field_rect);

    let item_h = (24.0 * scale) as i32;
    let search_h = (30.0 * scale) as i32;
    let key_items = state.param_editor.items_for_open_slot();
    let visible_items = state
        .param_editor
        .dropdown
        .visible_items(&key_items, key_dropdown_visible_rows());
    let list_rows = visible_items.len().max(1);
    let popup_rect = RECT {
        left: slot_rect.left,
        top: slot_rect.bottom,
        right: (slot_rect.left + (220.0 * scale) as i32).min(state.win_w - pad),
        bottom: slot_rect.bottom + search_h + list_rows as i32 * item_h,
    };
    if x >= popup_rect.left && x < popup_rect.right && y >= popup_rect.top && y < popup_rect.bottom
    {
        if y < popup_rect.top + search_h {
            return None;
        }
        let row = ((y - popup_rect.top - search_h) / item_h) as usize;
        if let Some(item) = visible_items.get(row) {
            return Some(item.id);
        }
    }
    None
}

//! Popup lifecycle and the Win32 window procedure boundary for the binding
//! editor popup.
//!
//! Owns window class registration, window creation, the modal message loop,
//! and cleanup. Unsafe Win32 boundary code lives here (and in `events`).

use std::ffi::c_void;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use crate::config::editor_binding_popup::events::binding_popup_wndproc;
use crate::config::editor_binding_popup::layout::{POPUP_HEIGHT_BASE, POPUP_WIDTH_BASE};
use crate::config::editor_binding_popup::paint::paint_binding_popup;
use crate::config::editor_binding_popup::state::{BindingPopupHit, BindingPopupState};
use crate::config::editor_key_combo::KeyComboEditorState;
use crate::config::editor_layout::{EDITOR_ACTION_NAMES, editor_action_desc};
use crate::config::editor_search_dropdown::{SearchDropdownItem, SearchDropdownState};
use crate::config::editor_state::SettingsState;
use crate::config::editor_theme::to_utf16_z;

// ── Window registration ───────────────────────────────────────────────

static POPUP_CLASS: std::sync::OnceLock<u16> = std::sync::OnceLock::new();

fn ensure_popup_class() -> u16 {
    *POPUP_CLASS.get_or_init(|| {
        let hinst: HINSTANCE = unsafe { HINSTANCE(GetModuleHandleW(None).unwrap_or_default().0) };
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
    parent_ptr: *mut SettingsState,
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
    let hinst: HINSTANCE = unsafe { HINSTANCE(GetModuleHandleW(None).unwrap_or_default().0) };

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

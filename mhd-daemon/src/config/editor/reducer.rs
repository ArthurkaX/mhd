//! Editor events applied to `SettingsState` — pure mutation helpers that
//! do not own any Win32 window lifecycle of their own.

use std::ffi::c_void;

use windows::Win32::Foundation::{HINSTANCE, POINT, RECT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use super::*;
use crate::core::action::ActionCategory;

// ═══════════════════════════════════════════════════════════════════════
// Inline editing helpers
// ═══════════════════════════════════════════════════════════════════════

#[allow(dead_code)] // kept for editor panel param editing
pub(crate) fn spawn_inline_edit(state: &mut SettingsState, idx: usize, rc: RECT) {
    if let Some(old) = state.param_edit_popup.take() {
        unsafe {
            let _ = DestroyWindow(old);
        }
    }

    let mut pt = POINT {
        x: rc.left,
        y: rc.top,
    };
    unsafe {
        let _ = ClientToScreen(state.hwnd, &mut pt);
    }

    let w = rc.right - rc.left;
    let h = rc.bottom - rc.top;

    let initial_text = state.bindings[idx].param.clone();
    let mut text_buf = [0u16; 1024];
    let len = initial_text.encode_utf16().count().min(1023);
    for (i, c) in initial_text.encode_utf16().take(len).enumerate() {
        text_buf[i] = c;
    }
    text_buf[len] = 0;

    let param_bg = state.theme.surface.blend_over(state.theme.background);
    let text_color = param_bg.contrasting_text_color();

    let info = ParamEditCreateInfo {
        state_ptr: state as *mut SettingsState,
        idx,
        width: w,
        height: h,
        initial_text: text_buf,
        text_color,
        brush_color: param_bg,
    };

    let cls_name = to_utf16_z("mhd_param_edit_popup_cls");
    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let hinstance: HINSTANCE = hinst.into();

    let popup = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR::from_raw(cls_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP | WS_BORDER,
            pt.x,
            pt.y,
            w,
            h,
            state.hwnd,
            HMENU::default(),
            hinstance,
            Some(&info as *const _ as *const c_void),
        )
    };

    match popup {
        Ok(h) => {
            state.param_edit_popup = Some(h);
            state.param_edit_idx = Some(idx);
            unsafe {
                let _ = ShowWindow(h, SW_SHOW);
            }
        }
        Err(_) => {
            state.edit_idx = Some(idx);
            state.edit_text = state.bindings[idx].param.clone();
            state.edit_cursor = state.bindings[idx].param.len();
            state.edit_select_start = None;
            state.edit_old_value = state.bindings[idx].param.clone();
        }
    }
}

pub(crate) fn finish_inline_edit(state: &mut SettingsState) {
    if let Some(field) = state.proxy_editing_field.take() {
        let val = std::mem::take(&mut state.edit_text);
        match field {
            ProxyEditField::AnthropicKey => state.anthropic_key = val,
            ProxyEditField::BindAddress => state.proxy_bind_address = val,
        }
        state.edit_cursor = 0;
        state.edit_select_start = None;
        state.edit_old_value.clear();
        paint_settings(state.hwnd, state as *mut SettingsState, &state.layout);
        return;
    }
    if let Some(idx) = state.edit_idx.take() {
        if state.expanded_idx == Some(idx) {
            state.acc_param = std::mem::take(&mut state.edit_text);
        } else {
            state.bindings[idx].param = std::mem::take(&mut state.edit_text);
        }
        state.edit_cursor = 0;
        state.edit_select_start = None;
        state.edit_old_value.clear();
        paint_settings(state.hwnd, state as *mut SettingsState, &state.layout);
    }
}

pub(crate) fn cancel_inline_edit(state: &mut SettingsState) {
    if state.proxy_editing_field.take().is_some() {
        state.edit_text.clear();
        state.edit_cursor = 0;
        state.edit_select_start = None;
        paint_settings(state.hwnd, state as *mut SettingsState, &state.layout);
        return;
    }
    if let Some(idx) = state.edit_idx.take() {
        if state.expanded_idx == Some(idx) {
            state.acc_param = std::mem::take(&mut state.edit_old_value);
        } else {
            state.bindings[idx].param = std::mem::take(&mut state.edit_old_value);
        }
        state.edit_text.clear();
        state.edit_cursor = 0;
        state.edit_select_start = None;
        paint_settings(state.hwnd, state as *mut SettingsState, &state.layout);
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Dropdown selection handlers
// ═══════════════════════════════════════════════════════════════════════

pub(crate) fn select_vision_model(state: &mut SettingsState, item_id: usize) {
    if item_id == 0 {
        state.vision_model = None;
    } else if let Some(item) = state.vision_model_items.get(item_id)
        && let Some((prov, model_name)) = item.label.split_once(" / ")
    {
        state.vision_model = Some(llm_proxy::config::ModelRef {
            provider: prov.to_string(),
            model: model_name.to_string(),
        });
    }
}

pub(crate) fn select_free_target(state: &mut SettingsState, item_id: usize) {
    if item_id == 0 {
        state.trim_free_target = String::new();
    } else if let Some(item) = state.trim_free_target_items.get(item_id) {
        state.trim_free_target = item.label.clone();
    }
}

pub(crate) fn select_head(state: &mut SettingsState, item_id: usize) {
    let group = match state.head_open_group {
        Some(g) => g,
        None => return,
    };
    if let Some(&v) = HEAD_SWEEP.get(item_id) {
        match group {
            HeadGroup::NativeBig => state.trim_toolresult_head = v,
            HeadGroup::NativeHaiku => state.trim_head_haiku = v,
            HeadGroup::Harness => state.trim_head_harness = v,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Action kind menu (cascading popup)
// ═══════════════════════════════════════════════════════════════════════

pub(crate) fn open_kind_menu(state: &mut SettingsState, idx: usize) {
    unsafe {
        let main_menu = CreatePopupMenu();
        let Ok(main_menu) = main_menu else { return };
        if main_menu == HMENU::default() {
            return;
        }

        let mut categories: Vec<ActionCategory> = Vec::new();
        for editor_idx in 0..EDITOR_ACTION_NAMES.len() {
            let cat = editor_action_desc(editor_idx).category;
            if !categories.contains(&cat) {
                categories.push(cat);
            }
        }

        for &cat in &categories {
            let sub = CreatePopupMenu();
            let Ok(sub) = sub else { continue };
            if sub == HMENU::default() {
                continue;
            }
            for editor_idx in 0..EDITOR_ACTION_NAMES.len() {
                let desc = editor_action_desc(editor_idx);
                if desc.category == cat {
                    let cmd = ID_ACTION_BASE + editor_idx;
                    let label = to_utf16_z(desc.label);
                    let _ = AppendMenuW(sub, MF_STRING, cmd, PCWSTR::from_raw(label.as_ptr()));
                }
            }
            let cat_label = to_utf16_z(cat.label());
            let _ = AppendMenuW(
                main_menu,
                MF_POPUP | MF_STRING,
                sub.0 as usize,
                PCWSTR::from_raw(cat_label.as_ptr()),
            );
        }

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);

        let chosen = TrackPopupMenu(
            main_menu,
            TPM_RETURNCMD | TPM_LEFTALIGN,
            pt.x,
            pt.y,
            0,
            state.hwnd,
            None,
        );
        let chosen = chosen.0 as usize;
        let _ = DestroyMenu(main_menu);

        if chosen >= ID_ACTION_BASE {
            let selected = chosen - ID_ACTION_BASE;
            if selected < EDITOR_ACTION_NAMES.len() {
                if state.bindings[idx].kind_idx != selected {
                    state.bindings[idx].kind_idx = selected;
                    let desc = editor_action_desc(selected);
                    state.bindings[idx].param = if desc.param_key.is_some() {
                        "5".to_string()
                    } else {
                        String::new()
                    };
                }
                paint_settings(state.hwnd, state as *mut SettingsState, &state.layout);
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Row click handler
// ═══════════════════════════════════════════════════════════════════════

pub(crate) fn handle_list_click(state: &mut SettingsState, idx: usize, x: i32, y: i32, row_y: i32) {
    let lay = state.layout;
    if x >= lay.win_w() - lay.pad() - lay.del_w()
        && x < lay.win_w() - lay.pad()
        && y >= row_y + (lay.row_h() - lay.del_w()) / 2
        && y < row_y + (lay.row_h() + lay.del_w()) / 2
    {
        close_kind_popup(state);
        state.bindings.remove(idx);
        if let Some((ri_idx, is_trig)) = state.recording_info {
            if ri_idx == idx {
                state.recording_info = None;
                crate::hook::set_recording_window(None);
            } else if ri_idx > idx {
                state.recording_info = Some((ri_idx - 1, is_trig));
            }
        }
        state.expanded_idx = None;
        paint_settings(state.hwnd, state as *mut SettingsState, &lay);
    }
}

//! Win32 window procedure for the binding editor popup.
//!
//! Native messages are converted into popup events: dropdown toggles, key
//! recording start/stop, param text editing, dropdown filtering, and Save /
//! Cancel. All mutation goes through the `state` transitions; rendering is
//! delegated to `paint`.

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::editor_binding_popup::hittest::hit_test_popup;
use crate::config::editor_binding_popup::layout::{
    POPUP_HEADER_HEIGHT_BASE, action_dropdown_visible_rows, key_dropdown_visible_rows,
};
use crate::config::editor_binding_popup::paint::paint_binding_popup;
use crate::config::editor_binding_popup::params::{
    default_param_for_schema, key_to_string, pick_param_file,
};
use crate::config::editor_binding_popup::state::{
    BindingPopupHit, BindingPopupState, cancel_param_edit, commit_param_edit, open_kind_dropdown,
};
use crate::config::editor_key_combo::KeyComboEditorState;
use crate::config::editor_layout::{WM_MOUSELEAVE, editor_action_desc};
use crate::config::text_cursor;
use crate::core::action::ActionParamSchema;
use crate::hook::{WM_BINDING_CAPTURED, set_recording_window};

pub(crate) unsafe extern "system" fn binding_popup_wndproc(
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
                            set_recording_window(Some(hwnd));
                        } else {
                            set_recording_window(None);
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
                            set_recording_window(Some(hwnd));
                        } else {
                            set_recording_window(None);
                        }
                        paint_binding_popup(hwnd, state_ptr);
                    }
                    BindingPopupHit::BrowseParam => {
                        // Open file picker for FilePath actions
                        state.is_recording_trigger = false;
                        state.is_recording_param = false;
                        set_recording_window(None);
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
                        set_recording_window(None);
                        state.saved = true;
                        state.should_close = true;
                    }
                    BindingPopupHit::CancelBtn => {
                        set_recording_window(None);
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
                            set_recording_window(None);
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
                                let start =
                                    text_cursor::prev(&state.param, state.param_edit_cursor);
                                let end = text_cursor::clamp(&state.param, state.param_edit_cursor);
                                if start < end {
                                    state.param.drain(start..end);
                                    state.param_edit_cursor = start;
                                }
                                paint_binding_popup(hwnd, state_ptr);
                            }
                            0x2E => {
                                // Delete
                                let start =
                                    text_cursor::clamp(&state.param, state.param_edit_cursor);
                                let end = text_cursor::next(&state.param, start);
                                if start < end {
                                    state.param.drain(start..end);
                                }
                                state.param_edit_cursor = start;
                                paint_binding_popup(hwnd, state_ptr);
                            }
                            0x25 => {
                                // Left arrow
                                state.param_edit_cursor =
                                    text_cursor::prev(&state.param, state.param_edit_cursor);
                                paint_binding_popup(hwnd, state_ptr);
                            }
                            0x27 => {
                                // Right arrow
                                state.param_edit_cursor =
                                    text_cursor::next(&state.param, state.param_edit_cursor);
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
                            let at = text_cursor::clamp(&state.param, state.param_edit_cursor);
                            state.param.insert(at, ch);
                            state.param_edit_cursor = at + ch.len_utf8();
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
                        set_recording_window(None);
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
                        set_recording_window(None);
                        paint_binding_popup(hwnd, state_ptr);
                    }
                }
                LRESULT(0)
            }

            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

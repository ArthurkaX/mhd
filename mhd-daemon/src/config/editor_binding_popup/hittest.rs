//! Hit-testing for the binding editor popup.
//!
//! Maps a mouse position inside the popup to a `BindingPopupHit` target, and
//! resolves which dropdown item (action kind, trigger key, param key) is under
//! the cursor. Open dropdown overlays get first chance at the hit-test.

use windows::Win32::Foundation::RECT;

use crate::config::editor_binding_popup::layout::{
    POPUP_BIND_BUTTON_WIDTH, POPUP_FIELD_HEIGHT, POPUP_FOOTER_HEIGHT_BASE,
    POPUP_HEADER_HEIGHT_BASE, POPUP_LABEL_WIDTH, POPUP_PADDING, POPUP_ROW_HEIGHT,
    action_dropdown_visible_rows, key_dropdown_visible_rows, trigger_slot_rects,
};
use crate::config::editor_binding_popup::state::{BindingPopupHit, BindingPopupState};
use crate::config::editor_layout::editor_action_desc;
use crate::core::action::ActionParamSchema;

pub(crate) fn hit_test_popup(state: &BindingPopupState, x: i32, y: i32) -> BindingPopupHit {
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

    let slot = state.trigger_editor.open_slot?;
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

    let slot = state.param_editor.open_slot?;
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

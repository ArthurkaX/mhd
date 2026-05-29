//! State types and transitions for the settings editor.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use windows::Win32::Foundation::HWND;

use crate::app::AppHandle;
use crate::core::native_theme::NativeTheme;
use crate::config::editor_layout::Layout;

// ── UI Binding (row data) ─────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct UIBinding {
    pub trigger: String,
    /// Index into [`EDITOR_ACTION_NAMES`].
    pub kind_idx: usize,
    pub param: String,
    pub is_recording_trigger: bool,
    pub is_recording_param: bool,
}

// ── Page enum ──────────────────────────────────────────────────────

/// Active page in the settings editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsPage {
    General,
    Shortcuts,
    Advanced,
}

// ── Hit‑test results ──────────────────────────────────────────────

/// Result from the centralized hit‑test function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsHit {
    None,
    Tab(usize),
    ThemeCombo,
    AutostartToggle,
    ApplyBtn,
    CloseBtn,
    AddBtn,
    /// Click on a shortcut overview row to edit it.
    RowClick(usize),
    /// Delete button on a shortcut row (with confirmation).
    RowDelete(usize),
    Scrollbar,
    /// Button on the Advanced page (index identifies the button).
    AdvancedButton(usize),
    // ── Accordion editor (inline, expanded row) ──────────────
    AccordionTriggerField,
    AccordionRecordBtn,
    AccordionActionBtn,
    AccordionParamField,
    AccordionParamRecordBtn,
    AccordionSaveBtn,
    AccordionCancelBtn,
    AccordionDeleteBtn,
}

// ── Hover target ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoverTarget {
    None,
    Tab(usize),
    ThemeCombo,
    AutostartToggle,
    ApplyBtn,
    CloseBtn,
    AddBtn,
    RowClick(usize),
    RowDelete(usize),
    Scrollbar,
    /// Hover target for a button on the Advanced page.
    AdvancedBtn(usize),
    // ── Accordion editor hovers ────────────────────────────────
    AccordionTriggerField,
    AccordionRecordBtn,
    AccordionActionBtn,
    AccordionParamField,
    AccordionParamRecordBtn,
    AccordionSaveBtn,
    AccordionCancelBtn,
    AccordionDeleteBtn,
}

// ── Button style ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    Primary,
    Secondary,
    DangerGhost,
    #[allow(dead_code)]
    TriggerPlate,
}

// ── Param edit popup create info ──────────────────────────────────

use crate::core::native_theme::Argb;

/// Data passed via `CREATESTRUCTW.lpCreateParams` to the popup.
#[repr(C)]
pub struct ParamEditCreateInfo {
    pub state_ptr: *mut SettingsState,
    pub idx: usize,
    pub width: i32,
    pub height: i32,
    pub initial_text: [u16; 1024],
    pub text_color: Argb,
    pub brush_color: Argb,
}

// ── Main editor state ─────────────────────────────────────────────

pub struct SettingsState {
    pub handle: AppHandle,
    pub theme: NativeTheme,
    pub hwnd: HWND,
    pub layout: Layout,
    pub active_section: SettingsPage,
    /// Theme names for the combo box
    pub theme_names: Vec<String>,
    /// Currently selected theme index
    pub theme_sel: usize,
    /// Currently hovered index in the popup
    pub hover_sel: Option<usize>,
    /// Combo popup window (when open)
    pub combo_popup: Option<HWND>,
    /// Whether the combo popup is open
    pub combo_open: Arc<AtomicBool>,

    /// Autostart at user logon (via scheduled task)
    pub autostart: bool,

    /// List of bindings being edited
    pub bindings: Vec<UIBinding>,
    /// Vertical scroll offset per section
    #[allow(dead_code)] // reserved for General tab scrolling
    pub general_scroll_y: i32,
    pub bindings_scroll_y: i32,
    /// Currently recording (binding_idx, is_trigger)
    pub recording_info: Option<(usize, bool)>,
    /// Index of binding with the accordion editor expanded (None = collapsed)
    pub expanded_idx: Option<usize>,
    /// Current trigger text in the accordion editor
    pub acc_trigger: String,
    /// Current action kind index in the accordion editor
    pub acc_kind_idx: usize,
    /// Current parameter text in the accordion editor
    pub acc_param: String,
    /// Whether the accordion is recording a trigger
    pub acc_is_recording: bool,
    /// Whether the accordion is recording a param (key binding for replace_key)
    pub acc_is_recording_param: bool,
    /// Validation error shown in the accordion footer
    pub acc_save_error: Option<String>,
    /// Index of binding being edited inline
    pub edit_idx: Option<usize>,
    /// Buffer for the inline-edited text (no HWND child control — layered window compat)
    pub edit_text: String,
    /// Cursor position within edit_text
    pub edit_cursor: usize,
    /// Start of selection for inline edit (None = no selection)
    pub edit_select_start: Option<usize>,
    /// Previous value to restore on Escape
    pub edit_old_value: String,

    /// Hovered interactive target
    pub hovered_target: HoverTarget,
    /// Scroll dragging state
    pub is_dragging_scroll: bool,
    /// Scroll drag starting mouse Y position
    pub scroll_drag_start_y: i32,
    /// Scroll drag starting scroll offset
    pub scroll_drag_start_offset: i32,

    /// Popup window for inline parameter editing (RichEdit child inside).
    pub param_edit_popup: Option<HWND>,
    /// Binding index being edited in the param edit popup.
    pub param_edit_idx: Option<usize>,
}

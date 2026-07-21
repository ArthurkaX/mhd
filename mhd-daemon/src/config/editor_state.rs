//! State types and transitions for the settings editor.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use windows::Win32::Foundation::HWND;

use crate::app::AppHandle;
use crate::config::editor_layout::Layout;
use crate::config::editor_search_dropdown::{SearchDropdownItem, SearchDropdownState};
use crate::core::native_theme::NativeTheme;
use crate::overlays::keycast::KeycastPosition;

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
    LlmProxy,
    LlmTrim,
    Advanced,
}

// ── Hit‑test results ──────────────────────────────────────────────

/// One provider row in the LLM Proxy providers list.
#[derive(Debug, Clone)]
pub struct UiProvider {
    pub name: String,
    pub endpoint: String,
    pub api_key: String,
    pub models: Vec<String>,
}

/// Result from the centralized hit‑test function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsHit {
    None,
    Tab(usize),
    ThemeCombo,
    AutostartToggle,
    SaveBtn,
    ApplyBtn,
    CloseBtn,
    AddBtn,
    /// Click on a provider row to edit it.
    ProviderRow(usize),
    /// Edit button on a provider row.
    ProviderEditBtn(usize),
    /// Models button on a provider row.
    ProviderModelsBtn(usize),
    /// Delete button on a provider row.
    ProviderDelete(usize),
    /// Add a new provider.
    ProviderAddBtn,
    /// Click on a shortcut overview row to edit it.
    RowClick(usize),
    /// Delete button on a shortcut row (with confirmation).
    RowDelete(usize),
    Scrollbar,
    /// Button on the Advanced page (index identifies the button).
    AdvancedButton(usize),
    /// Browse button for Quick Note save path.
    NotesDirBrowseBtn,
    /// Browse button for Quick Draw save path.
    DrawDirBrowseBtn,
    /// KeyCast position preset.
    KeycastPositionBtn(usize),
    /// Decrease KeyCast display duration.
    KeycastDurationDown,
    /// Increase KeyCast display duration.
    KeycastDurationUp,
    /// Toggle KeyCast typing block on/off.
    KeycastShowTypingToggle,
    /// Decrease typing width.
    KeycastTypingWidthDown,
    /// Increase typing width.
    KeycastTypingWidthUp,
    /// Decrease typing duration.
    KeycastTypingDurationDown,
    /// Increase typing duration.
    KeycastTypingDurationUp,
    // ── Accordion editor (inline, expanded row) ──────────────
    AccordionTriggerField,
    AccordionRecordBtn,
    AccordionActionBtn,
    AccordionParamField,
    AccordionParamRecordBtn,
    AccordionSaveBtn,
    AccordionCancelBtn,
    AccordionDeleteBtn,
    /// Inline-editable Anthropic Key field.
    ProxyAnthropicKeyField,
    /// Inline-editable bind address field.
    ProxyBindAddressField,
    /// Reserved: hit-target for the proxy downgrade toggles (not yet wired).
    #[allow(dead_code)]
    ProxyOpusDowngradeToggle,
    /// Reserved: hit-target for the proxy downgrade toggles (not yet wired).
    #[allow(dead_code)]
    ProxySonnetDowngradeToggle,
    /// Vision model selector dropdown.
    VisionModelCombo,
    /// Vision model Test button.
    VisionTestBtn,
    /// Vision prompt edit button.
    VisionPromptBtn,
    /// Trim (request compression) toggle.
    TrimToggle,
    /// Free/cheap model selector dropdown.
    TrimFreeTargetCombo,
    /// Head-budget arrow button for Native-big group.
    HeadArrowNativeBig,
    /// Head-budget arrow button for Native-Haiku group.
    HeadArrowHaiku,
    /// Head-budget arrow button for Harness group.
    HeadArrowHarness,
    /// Calculate button that runs the Tune sweep on background thread.
    HeadCalculateBtn,
}

/// Which head-budget group the shared head dropdown is editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadGroup { NativeBig, NativeHaiku, Harness }

/// Sweep of head-budget values for the head dropdown.
pub const HEAD_SWEEP: [usize; 9] = [100, 200, 300, 500, 1000, 2000, 3000, 5000, 8000];

/// Canned (pre-measurement) explanation for a head value in a group.
/// Replaced by live Tune numbers in a later pass.
pub fn head_help_text(group: HeadGroup, head: usize) -> String {
    let tier = match group {
        HeadGroup::NativeBig => "big native tiers",
        HeadGroup::NativeHaiku => "Haiku",
        HeadGroup::Harness => "OpenAI harness",
    };
    match head {
        0..=200 => format!("Aggressive — smallest prompts for {tier} (est.)"),
        201..=1000 => format!("Balanced — good savings for {tier} (est.)"),
        1001..=3000 => format!("Conservative — safe default for {tier} (est.)"),
        _ => format!("Minimal trim — largest context for {tier} (est.)"),
    }
}

/// Which global proxy field is being inline-edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyEditField {
    AnthropicKey,
    BindAddress,
}

// ── Button style ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    Primary,
    Secondary,
    DangerGhost,
    /// Green ghost — always green regardless of theme.
    Success,
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

    /// Search dropdown items for theme selection
    pub theme_search_items: Vec<SearchDropdownItem>,
    /// Search dropdown state for theme selection
    pub theme_dropdown: SearchDropdownState,

    /// Autostart at user logon (via scheduled task)
    pub autostart: bool,

    /// Quick Note save directory
    pub notes_dir: PathBuf,

    /// Quick Draw save directory
    pub draw_dir: PathBuf,

    /// KeyCast overlay position
    pub keycast_position: KeycastPosition,

    /// KeyCast label display duration in milliseconds
    pub keycast_duration_ms: u64,

    /// Show typing block for single printable keystrokes.
    pub keycast_show_typing: bool,
    /// Typing block width in characters.
    pub keycast_typing_width_chars: u32,
    /// Typing block character duration in milliseconds.
    pub keycast_typing_duration_ms: u64,

    /// List of bindings being edited
    pub bindings: Vec<UIBinding>,
    /// List of providers being edited (LLM Proxy page).
    pub providers: Vec<UiProvider>,
    /// Anthropic API key for native passthrough (global proxy setting).
    pub anthropic_key: String,
    /// Proxy bind address like "127.0.0.1:3456".
    pub proxy_bind_address: String,
    /// Opus downgrade toggle.
    pub opus_downgrade_enabled: bool,
    /// Sonnet downgrade toggle.
    pub sonnet_downgrade_enabled: bool,
    /// Vertical scroll offset for the content area (all pages).
    pub content_scroll_y: i32,
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
    /// Which proxy field is being inline-edited (None = no proxy field editing).
    pub proxy_editing_field: Option<ProxyEditField>,

    /// Hovered interactive target
    pub hovered_target: SettingsHit,
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

    /// Tab titles in display order. Used by hit-test and paint to size and
    /// label the tab bar without hardcoding counts.
    pub tab_titles: Vec<&'static str>,
    /// Hit-testable rects for the active page, rebuilt on every paint.
    /// Looked up linearly by `hit_test_settings`.
    pub hit_regions: Vec<crate::config::editor_control::HitRegion>,

    /// Selected vision model reference (provider + model id).
    pub vision_model: Option<llm_proxy::config::ModelRef>,
    /// Custom prompt used for the vision screenshot request.
    pub vision_prompt: String,
    /// Items for the vision model search dropdown.
    pub vision_model_items: Vec<SearchDropdownItem>,
    /// Search dropdown state for vision model selection.
    pub vision_model_dropdown: SearchDropdownState,
    /// Vision test button state: "Test", "Testing...", "Passed", or error message.
    pub vision_test_status: String,
    /// Whether a vision test is currently running.
    pub vision_test_running: bool,
    /// Enable native request compression.
    pub trim_enabled: bool,
    /// Tool description max chars for trim.
    pub trim_tool_desc_chars: usize,
    /// Tool result head chars for trim.
    pub trim_toolresult_head: usize,
    /// Tool result tail chars for trim.
    pub trim_toolresult_tail: usize,
    /// Trim whitespace compression toggle.
    pub trim_ws_enabled: bool,
    /// Trim strip-thinking toggle.
    pub trim_strip_thinking: bool,
    /// Free/cheap trim target model id (empty = off).
    pub trim_free_target: String,
    /// Items for the free target search dropdown.
    pub trim_free_target_items: Vec<SearchDropdownItem>,
    /// Search dropdown state for free target selection.
    pub trim_free_target_dropdown: SearchDropdownState,
    /// Haiku group head-budget value.
    pub trim_head_haiku: usize,
    /// Harness group head-budget value.
    pub trim_head_harness: usize,
    /// Items for the shared head-budget dropdown.
    pub head_items: Vec<SearchDropdownItem>,
    /// Shared head-budget dropdown state.
    pub head_dropdown: SearchDropdownState,
    /// Which group the head dropdown is currently editing (None = closed).
    pub head_open_group: Option<HeadGroup>,
    /// Hovered row index in the open head dropdown (drives the description
    /// inset); None = nothing hovered / falls back to the selected row.
    pub head_hover_idx: Option<usize>,
}

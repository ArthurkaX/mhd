//! Layout constants and geometry computation for the settings editor.
//!
//! All constants are defined at a 96 DPI base and scaled at runtime by
//! the DPI factor.

// ── Layout constants (96 dpi base) ─────────────────────────────────

pub const WIN_WIDTH_BASE: i32 = 780;
pub const WIN_HEIGHT_BASE: i32 = 580;
pub const PADDING: i32 = 24;
pub const HEADER_HEIGHT_BASE: i32 = 64;
pub const FOOTER_HEIGHT_BASE: i32 = 52;
pub const ROW_HEIGHT_BASE: i32 = 32;
pub const LABEL_WIDTH_BASE: i32 = 120; // Increased for better alignment
pub const BTN_WIDTH_BASE: i32 = 100;
pub const BTN_HEIGHT_BASE: i32 = 30;
pub const COMBO_HIT_HEIGHT: i32 = 24;
pub const ROUND_RADIUS_BASE: f32 = 14.0;
pub const SECTION_GAP_BASE: i32 = 16;
pub const CONTROL_ROW_HEIGHT_BASE: i32 = 40;
pub const SECTION_HEADER_HEIGHT_BASE: i32 = 28;

// Fonts
pub const FONT_TITLE_SIZE: i32 = 16;
pub const FONT_BODY_SIZE: i32 = 12;
pub const FONT_SMALL_SIZE: i32 = 10;

// ── Combo popup constants ──────────────────────────────────────────

pub const COMBO_POPUP_WIDTH: i32 = 260;
pub const COMBO_POPUP_ITEM_HEIGHT: i32 = 24;
pub const COMBO_POPUP_MAX_VISIBLE: i32 = 8;

// ── Tab bar constants ──────────────────────────────────────────────

pub const TAB_WIDTH_BASE: i32 = 90;
pub const TAB_HEIGHT_BASE: i32 = 28;
pub const TAB_BAR_GAP_BASE: i32 = 8;
pub const TAB_CONTENT_GAP_BASE: i32 = 12;

// ── Editor action names ────────────────────────────────────────────

/// Actions exposed in the settings editor, by stable TOML action name.
///
/// Do not store positions from `ALL_ACTIONS` here: adding/reordering actions in
/// the registry must not shift editor choices (that caused Quick Note to become
/// Quit mhd in saved configs).
pub const EDITOR_ACTION_NAMES: &[&str] = &[
    "replace_key",
    "run_program",
    "run_ps",
    "brightness_up",
    "brightness_down",
    "show_monitor_panel",
    "show_volume_mixer",
    "media_volume_up",
    "media_volume_down",
    "media_mute",
    "media_play_pause",
    "media_stop",
    "media_last_track",
    "media_next_track",
    "toggle_topmost",
    "toggle_suspend_on_blur",
    "toggle_throttle_on_blur",
    "power_actions",
    "quick_draw",
    "quick_note",
    "pomodoro",
    "switch_power_plan",
    "show_cpu_panel",
    "quit",
];

// ── Advanced page constants ────────────────────────────────────────

pub const ADVANCED_BUTTONS: &[(&str, &str)] = &[
    (
        "Open Config File",
        "Edit the TOML configuration file directly",
    ),
    (
        "Open Config Folder",
        "Open the configuration directory in Explorer",
    ),
    ("Open Blackbox Logs", "Open the blackbox log directory"),
    ("Open Crash Log", "Open the most recent crash log"),
    (
        "Reset Shortcuts",
        "Restore all shortcuts to their default values",
    ),
    (
        "Reset All Settings",
        "Restore all settings to factory defaults",
    ),
];

/// Group definitions: (name, start_index, end_index_exclusive, is_danger)
pub const ADVANCED_GROUPS: &[(&str, usize, usize, bool)] = &[
    ("Config Files", 0, 2, false),
    ("Logs", 2, 4, false),
    ("Reset", 4, 6, true),
];

// ── Custom messages ────────────────────────────────────────────────

pub const WM_PARAM_EDIT_COMMIT: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 1;

/// Width across mouse-leave messages (not defined in older SDKs).
pub const WM_MOUSELEAVE: u32 = 0x02A3;

/// Base menu command ID for the action kind popup.
pub const ID_ACTION_BASE: usize = 1000;

// ── Editor action helpers ─────────────────────────────────────────

/// Get the action descriptor for an editor action by its index in [`EDITOR_ACTION_NAMES`].
pub fn editor_action_desc(editor_idx: usize) -> &'static crate::core::action::ActionDescriptor {
    let name = EDITOR_ACTION_NAMES
        .get(editor_idx)
        .copied()
        .unwrap_or("quit");
    crate::core::action::ALL_ACTIONS
        .iter()
        .find(|d| d.name == name)
        .unwrap_or_else(|| {
            crate::core::action::ALL_ACTIONS
                .iter()
                .find(|d| d.name == "quit")
                .unwrap()
        })
}

/// Get the editor index for an action name, falling back to the "quit" action.
pub fn editor_index_for_action_name(name: &str) -> usize {
    EDITOR_ACTION_NAMES
        .iter()
        .position(|n| *n == name)
        .unwrap_or_else(|| {
            EDITOR_ACTION_NAMES
                .iter()
                .position(|n| *n == "quit")
                .unwrap()
        })
}

// ── Scaled layout ──────────────────────────────────────────────────

/// Pre-computed geometry for the settings editor window.
///
/// All fields are in logical (DPI-scaled) pixels.
#[derive(Copy, Clone)]
pub struct Layout {
    pub scale: f32,
    pub win_w: i32,
    pub win_h: i32,
    pub pad: i32,
    pub header_h: i32,
    pub footer_h: i32,

    // Tab bar (horizontal, under header separator)
    pub tab_h: i32,
    pub tab_w: i32,
    pub tab_gap: i32,
    pub tab_bar_y: i32,

    // Content starts after tab bar + gap
    pub content_y: i32,

    // Sections
    pub appearance_y: i32,
    pub shortcuts_y: i32,

    // Appearance controls
    pub label_w: i32,
    pub combo_x: i32,
    pub combo_w: i32,
    pub combo_y: i32,
    pub autostart_y: i32,
    pub arrow_x: i32,
    pub arrow_w: i32,

    // Table columns
    pub trig_w: i32,
    pub kind_w: i32,
    pub del_w: i32,

    // Shortcuts list
    pub list_y: i32,
    pub list_h: i32,
    pub row_h: i32,
    pub accordion_h: i32,

    // Footer buttons
    pub btn_h: i32,
    pub btn_w: i32,
    pub btn_y: i32,
    pub apply_x: i32,
    pub close_x: i32,

    pub radius: i32,
}

// SAFETY: Layout contains only plain i32/f32 fields, no raw pointers.
unsafe impl Send for Layout {}
unsafe impl Sync for Layout {}

/// Compute the scaled layout for a given DPI scaling factor.
pub fn compute_layout(scale: f32) -> Layout {
    let pad = (PADDING as f32 * scale) as i32;
    let header_h = (HEADER_HEIGHT_BASE as f32 * scale) as i32;
    let footer_h = (FOOTER_HEIGHT_BASE as f32 * scale) as i32;
    let row_h = (ROW_HEIGHT_BASE as f32 * scale) as i32;
    let accordion_h = (160.0 * scale) as i32;
    let btn_h = (BTN_HEIGHT_BASE as f32 * scale) as i32;
    let btn_w = (BTN_WIDTH_BASE as f32 * scale) as i32;
    let win_w = (WIN_WIDTH_BASE as f32 * scale) as i32;
    let win_h = (WIN_HEIGHT_BASE as f32 * scale) as i32;

    // Horizontal tab bar
    let tab_h = (TAB_HEIGHT_BASE as f32 * scale) as i32;
    let tab_w = (TAB_WIDTH_BASE as f32 * scale) as i32;
    let tab_gap = (TAB_BAR_GAP_BASE as f32 * scale) as i32;
    let tab_bar_y = header_h + (4.0 * scale) as i32;
    let content_y = tab_bar_y + tab_h + (TAB_CONTENT_GAP_BASE as f32 * scale) as i32;

    let appearance_y = content_y;
    let label_w = (LABEL_WIDTH_BASE as f32 * scale) as i32;
    let combo_h = COMBO_HIT_HEIGHT.max((COMBO_HIT_HEIGHT as f32 * scale) as i32);
    let combo_x = pad + label_w + 8;
    let combo_w = (COMBO_POPUP_WIDTH as f32 * scale) as i32;
    let combo_y = appearance_y + (SECTION_HEADER_HEIGHT_BASE as f32 * scale) as i32;

    let shortcuts_y = content_y;
    let autostart_y = combo_y
        + (CONTROL_ROW_HEIGHT_BASE as f32 * scale) as i32
        + (SECTION_GAP_BASE as f32 * scale) as i32
        + (SECTION_HEADER_HEIGHT_BASE as f32 * scale) as i32;
    let list_y = shortcuts_y + (48.0 * scale) as i32;
    let list_h = (win_h - footer_h) - list_y - pad / 2;

    let trig_w = (160.0 * scale) as i32;
    let kind_w = (150.0 * scale) as i32;
    let del_w = (28.0 * scale) as i32;

    let btn_y = win_h - footer_h + (footer_h - btn_h) / 2;

    let radius = (ROUND_RADIUS_BASE * scale) as i32;

    Layout {
        scale,
        win_w,
        win_h,
        pad,
        header_h,
        footer_h,
        tab_h,
        tab_w,
        tab_gap,
        tab_bar_y,
        content_y,
        appearance_y,
        shortcuts_y,
        label_w,
        combo_x,
        combo_w,
        combo_y,
        arrow_x: combo_x + combo_w - combo_h,
        arrow_w: combo_h,
        autostart_y,
        trig_w,
        kind_w,
        del_w,
        btn_h,
        btn_w,
        btn_y,
        apply_x: win_w - pad - btn_w,
        close_x: win_w - pad - btn_w * 2 - (8.0 * scale) as i32,
        radius,
        list_y,
        list_h,
        row_h,
        accordion_h,
    }
}

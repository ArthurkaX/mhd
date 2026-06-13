//! Layout constants and geometry computation for the settings editor.
//!
//! All constants are defined at a 96 DPI base and scaled at runtime by
//! the DPI factor.

#![allow(dead_code)]
//!
//! `Layout` is a composition of [`CommonLayout`] (window, header, footer,
//! tab bar, button bar) plus per-page sub-structs. This mirrors the page
//! structure of the editor itself — each page owns its own geometry, and
//! the common section is reused across all of them.

// ── Layout constants (96 dpi base) ─────────────────────────────────

pub const WIN_WIDTH_BASE: i32 = 780;
pub const WIN_HEIGHT_BASE: i32 = 556;
pub const PADDING: i32 = 24;
pub const HEADER_HEIGHT_BASE: i32 = 56;
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
pub const FONT_BODY_SIZE: i32 = 13;
pub const FONT_SMALL_SIZE: i32 = 11;

// ── Combo popup constants ──────────────────────────────────────────

pub const COMBO_POPUP_WIDTH: i32 = 260;
pub const COMBO_POPUP_ITEM_HEIGHT: i32 = 24;
pub const COMBO_POPUP_MAX_VISIBLE: i32 = 8;

// ── Tab bar constants ──────────────────────────────────────────────

pub const TAB_WIDTH_BASE: i32 = 100;
pub const TAB_HEIGHT_BASE: i32 = 26;
pub const TAB_BAR_GAP_BASE: i32 = 4;
pub const TAB_CONTENT_GAP_BASE: i32 = 8;

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
    "show_llm_models",
    "toggle_llm_proxy",
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
#[allow(dead_code)]
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

// ── Layout sub-structs ─────────────────────────────────────────────

/// Geometry shared by all pages: window frame, header, tab bar, footer
/// button bar, plus layout-wide constants (padding, label width, etc.).
#[derive(Copy, Clone)]
pub struct CommonLayout {
    pub scale: f32,
    pub win_w: i32,
    pub win_h: i32,
    pub pad: i32,
    pub header_h: i32,
    pub footer_h: i32,

    // Tab bar
    pub tab_h: i32,
    pub tab_w: i32,
    pub tab_gap: i32,
    pub tab_bar_y: i32,

    // Content area (top of page-specific drawing)
    pub content_y: i32,

    // Footer buttons
    pub btn_h: i32,
    pub btn_w: i32,
    pub btn_y: i32,
    pub save_x: i32,
    pub close_x: i32,
    pub apply_x: i32,

    pub radius: i32,
    pub label_w: i32,
    pub section_gap: i32,
    pub section_header_h: i32,
    pub control_row_h: i32,
    pub content_visible_h: i32,
    pub scrollbar_w: i32,
}

/// Geometry specific to the General (Appearance, Startup, Quick Note) page.
#[derive(Copy, Clone)]
pub struct GeneralLayout {
    pub appearance_y: i32,
    pub combo_x: i32,
    pub combo_w: i32,
    pub combo_y: i32,
    pub arrow_x: i32,
    pub arrow_w: i32,
    pub autostart_y: i32,
    pub toggle_w: i32,
    pub toggle_h: i32,
    pub notes_path_y: i32,
    pub draw_path_y: i32,
    pub browse_btn_w: i32,
    pub notes_row_h: i32,
}

/// Geometry specific to the Shortcuts page (list, accordion, scrollbar).
#[derive(Copy, Clone)]
pub struct ShortcutsLayout {
    pub shortcuts_y: i32,
    pub list_y: i32,
    pub list_h: i32,
    pub row_h: i32,
    pub accordion_h: i32,
    pub trig_w: i32,
    pub kind_w: i32,
    pub del_w: i32,
    pub scroll_w: i32,
    pub table_header_y: i32,
}

/// Geometry specific to the Advanced page (action buttons list).
#[derive(Copy, Clone)]
pub struct AdvancedLayout {
    pub top_y: i32,
    pub btn_h: i32,
    pub btn_gap: i32,
    pub btn_w: i32,
}

/// Geometry specific to the LLM Proxy page (providers list + future sections).
#[derive(Copy, Clone)]
pub struct LlmProxyLayout {
    pub top_y: i32,
    /// Y offset of the "Proxy Settings" section.
    pub proxy_y: i32,
    /// Height of the proxy settings section (fields + header).
    pub proxy_h: i32,
    /// Y offset of the provider list (below the proxy settings).
    pub list_y: i32,
    /// Available height of the provider list area.
    pub list_h: i32,
    /// Single row height.
    pub row_h: i32,
    /// Width of the name column.
    pub name_w: i32,
    /// Width of the delete button.
    pub del_w: i32,
}

/// Top-level layout composition.
#[derive(Copy, Clone)]
pub struct Layout {
    pub common: CommonLayout,
    pub general: GeneralLayout,
    pub shortcuts: ShortcutsLayout,
    pub advanced: AdvancedLayout,
    pub llm_proxy: LlmProxyLayout,
}

// SAFETY: Layout contains only plain i32/f32 fields, no raw pointers.
unsafe impl Send for Layout {}
unsafe impl Sync for Layout {}

/// Backward-compat shim. Old code reads `lay.win_w`, `lay.scale`, etc.
/// These accessors forward to the new sub-struct fields. New code should
/// read `lay.common.win_w` etc. directly.
impl Layout {
    pub fn win_w(&self) -> i32 {
        self.common.win_w
    }
    pub fn win_h(&self) -> i32 {
        self.common.win_h
    }
    pub fn scale(&self) -> f32 {
        self.common.scale
    }
    pub fn pad(&self) -> i32 {
        self.common.pad
    }
    pub fn header_h(&self) -> i32 {
        self.common.header_h
    }
    pub fn footer_h(&self) -> i32 {
        self.common.footer_h
    }
    pub fn radius(&self) -> i32 {
        self.common.radius
    }
    pub fn label_w(&self) -> i32 {
        self.common.label_w
    }
    pub fn section_gap(&self) -> i32 {
        self.common.section_gap
    }
    pub fn section_header_h(&self) -> i32 {
        self.common.section_header_h
    }
    pub fn control_row_h(&self) -> i32 {
        self.common.control_row_h
    }
    pub fn tab_h(&self) -> i32 {
        self.common.tab_h
    }
    pub fn tab_w(&self) -> i32 {
        self.common.tab_w
    }
    pub fn tab_gap(&self) -> i32 {
        self.common.tab_gap
    }
    pub fn tab_bar_y(&self) -> i32 {
        self.common.tab_bar_y
    }
    pub fn content_y(&self) -> i32 {
        self.common.content_y
    }
    pub fn btn_h(&self) -> i32 {
        self.common.btn_h
    }
    pub fn btn_w(&self) -> i32 {
        self.common.btn_w
    }
    pub fn btn_y(&self) -> i32 {
        self.common.btn_y
    }
    pub fn apply_x(&self) -> i32 {
        self.common.apply_x
    }
    pub fn close_x(&self) -> i32 {
        self.common.close_x
    }
    pub fn save_x(&self) -> i32 {
        self.common.save_x
    }
    pub fn content_visible_h(&self) -> i32 {
        self.common.content_visible_h
    }
    pub fn scrollbar_w(&self) -> i32 {
        self.common.scrollbar_w
    }

    // General page
    pub fn appearance_y(&self) -> i32 {
        self.general.appearance_y
    }
    pub fn combo_x(&self) -> i32 {
        self.general.combo_x
    }
    pub fn combo_w(&self) -> i32 {
        self.general.combo_w
    }
    pub fn combo_y(&self) -> i32 {
        self.general.combo_y
    }
    pub fn arrow_x(&self) -> i32 {
        self.general.arrow_x
    }
    pub fn arrow_w(&self) -> i32 {
        self.general.arrow_w
    }
    pub fn autostart_y(&self) -> i32 {
        self.general.autostart_y
    }
    pub fn toggle_w(&self) -> i32 {
        self.general.toggle_w
    }
    pub fn toggle_h(&self) -> i32 {
        self.general.toggle_h
    }
    pub fn notes_path_y(&self) -> i32 {
        self.general.notes_path_y
    }
    pub fn draw_path_y(&self) -> i32 {
        self.general.draw_path_y
    }
    pub fn browse_btn_w(&self) -> i32 {
        self.general.browse_btn_w
    }
    pub fn notes_row_h(&self) -> i32 {
        self.general.notes_row_h
    }

    // Shortcuts page
    pub fn shortcuts_y(&self) -> i32 {
        self.shortcuts.shortcuts_y
    }
    pub fn list_y(&self) -> i32 {
        self.shortcuts.list_y
    }
    pub fn list_h(&self) -> i32 {
        self.shortcuts.list_h
    }
    pub fn row_h(&self) -> i32 {
        self.shortcuts.row_h
    }
    pub fn accordion_h(&self) -> i32 {
        self.shortcuts.accordion_h
    }
    pub fn trig_w(&self) -> i32 {
        self.shortcuts.trig_w
    }
    pub fn kind_w(&self) -> i32 {
        self.shortcuts.kind_w
    }
    pub fn del_w(&self) -> i32 {
        self.shortcuts.del_w
    }
    pub fn scroll_w(&self) -> i32 {
        self.shortcuts.scroll_w
    }
    pub fn table_header_y(&self) -> i32 {
        self.shortcuts.table_header_y
    }

    // LLM Proxy page
    pub fn provider_list_y(&self) -> i32 {
        self.llm_proxy.list_y
    }
    pub fn provider_list_h(&self) -> i32 {
        self.llm_proxy.list_h
    }
    pub fn provider_row_h(&self) -> i32 {
        self.llm_proxy.row_h
    }
    pub fn provider_name_w(&self) -> i32 {
        self.llm_proxy.name_w
    }
    pub fn provider_del_w(&self) -> i32 {
        self.llm_proxy.del_w
    }
}

/// Compute the scaled layout for a given DPI scaling factor.
pub fn compute_layout(scale: f32) -> Layout {
    let common = compute_common_layout(scale);
    let general = compute_general_layout(&common);
    let shortcuts = compute_shortcuts_layout(&common);
    let advanced = compute_advanced_layout(&common);
    let llm_proxy = compute_llm_proxy_layout(&common);

    Layout {
        common,
        general,
        shortcuts,
        advanced,
        llm_proxy,
    }
}

fn compute_common_layout(scale: f32) -> CommonLayout {
    let pad = (PADDING as f32 * scale) as i32;
    let header_h = (HEADER_HEIGHT_BASE as f32 * scale) as i32;
    let footer_h = (FOOTER_HEIGHT_BASE as f32 * scale) as i32;
    let btn_h = (BTN_HEIGHT_BASE as f32 * scale) as i32;
    let btn_w = (BTN_WIDTH_BASE as f32 * scale) as i32;
    let win_w = (WIN_WIDTH_BASE as f32 * scale) as i32;
    let win_h = (WIN_HEIGHT_BASE as f32 * scale) as i32;

    let tab_h = (TAB_HEIGHT_BASE as f32 * scale) as i32;
    let tab_w = (TAB_WIDTH_BASE as f32 * scale) as i32;
    let tab_gap = (TAB_BAR_GAP_BASE as f32 * scale) as i32;
    let tab_bar_y = (header_h - tab_h) / 2;
    let content_y = header_h + (TAB_CONTENT_GAP_BASE as f32 * scale) as i32;

    let btn_y = win_h - footer_h + (footer_h - btn_h) / 2;
    let label_w = (LABEL_WIDTH_BASE as f32 * scale) as i32;
    let section_gap = (SECTION_GAP_BASE as f32 * scale) as i32;
    let section_header_h = (SECTION_HEADER_HEIGHT_BASE as f32 * scale) as i32;
    let control_row_h = (CONTROL_ROW_HEIGHT_BASE as f32 * scale) as i32;
    let radius = (ROUND_RADIUS_BASE * scale) as i32;

    let content_visible_h = btn_y - content_y - (8.0 * scale) as i32;
    let scrollbar_w = (8.0 * scale).max(6.0) as i32;

    CommonLayout {
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
        content_visible_h,
        scrollbar_w,
        btn_h,
        btn_w,
        btn_y,
        close_x: win_w - pad - btn_w, // rightmost
        apply_x: win_w - pad - btn_w * 2 - (8.0 * scale) as i32, // middle
        save_x: win_w - pad - btn_w * 3 - (16.0 * scale) as i32, // leftmost, highlighted
        radius,
        label_w,
        section_gap,
        section_header_h,
        control_row_h,
    }
}

fn compute_general_layout(c: &CommonLayout) -> GeneralLayout {
    let combo_h = COMBO_HIT_HEIGHT.max((COMBO_HIT_HEIGHT as f32 * c.scale) as i32);
    let combo_x = c.pad + c.label_w + 8;
    let combo_w = (COMBO_POPUP_WIDTH as f32 * c.scale) as i32;
    let appearance_y = c.content_y;
    let combo_y = appearance_y + c.section_header_h;

    let autostart_y = combo_y + c.control_row_h + c.section_gap + c.section_header_h;
    let toggle_w = (36.0 * c.scale) as i32;
    let toggle_h = (18.0 * c.scale) as i32;

    let notes_divider_y = autostart_y + (20.0 * c.scale) as i32 + c.section_gap / 2;
    let notes_header_y = notes_divider_y + c.section_gap / 2;
    let notes_path_y = notes_header_y + c.section_header_h;
    let notes_row_h = (28.0 * c.scale) as i32;
    let browse_btn_w = (80.0 * c.scale) as i32;

    let draw_divider_y = notes_path_y + notes_row_h + c.section_gap / 2;
    let draw_header_y = draw_divider_y + c.section_gap / 2;
    let draw_path_y = draw_header_y + c.section_header_h;

    GeneralLayout {
        appearance_y,
        combo_x,
        combo_w,
        combo_y,
        arrow_x: combo_x + combo_w - combo_h,
        arrow_w: combo_h,
        autostart_y,
        toggle_w,
        toggle_h,
        notes_path_y,
        draw_path_y,
        browse_btn_w,
        notes_row_h,
    }
}

fn compute_shortcuts_layout(c: &CommonLayout) -> ShortcutsLayout {
    let shortcuts_y = c.content_y;
    let list_y = shortcuts_y + (48.0 * c.scale) as i32;
    let list_h = (c.win_h - c.footer_h) - list_y - c.pad / 2;
    let row_h = (ROW_HEIGHT_BASE as f32 * c.scale) as i32;
    let accordion_h = (160.0 * c.scale) as i32;
    let trig_w = (160.0 * c.scale) as i32;
    let kind_w = (150.0 * c.scale) as i32;
    let del_w = (28.0 * c.scale) as i32;
    let scroll_w = (6.0 * c.scale) as i32;
    let table_header_y = list_y - (24.0 * c.scale) as i32;

    ShortcutsLayout {
        shortcuts_y,
        list_y,
        list_h,
        row_h,
        accordion_h,
        trig_w,
        kind_w,
        del_w,
        scroll_w,
        table_header_y,
    }
}

fn compute_advanced_layout(c: &CommonLayout) -> AdvancedLayout {
    AdvancedLayout {
        top_y: c.content_y,
        btn_h: (36.0 * c.scale) as i32,
        btn_gap: (8.0 * c.scale) as i32,
        btn_w: (260.0 * c.scale) as i32,
    }
}

fn compute_llm_proxy_layout(c: &CommonLayout) -> LlmProxyLayout {
    let section_h = (SECTION_HEADER_HEIGHT_BASE as f32 * c.scale) as i32;
    let row_h = (ROW_HEIGHT_BASE as f32 * c.scale) as i32;
    let gap = (8.0 * c.scale) as i32;
    let col_h = (16.0 * c.scale) as i32; // column header height
    // Proxy settings: header + Anthropic Key + Bind Address + Opus Downgrade section
    let proxy_h = section_h + row_h + gap + row_h + gap // Proxy Settings section
        + section_h + row_h + gap + row_h + gap // Opus Downgrade section
        + (gap / 2) + col_h + gap + (gap / 2) + 1; // provider list header area + divider
    let list_y = c.content_y + proxy_h;
    let list_h = c.content_visible_h - proxy_h;
    LlmProxyLayout {
        top_y: c.content_y,
        proxy_y: c.content_y,
        proxy_h,
        list_y,
        list_h,
        row_h,
        name_w: (260.0 * c.scale) as i32,
        del_w: (28.0 * c.scale) as i32,
    }
}

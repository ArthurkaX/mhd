use serde::Deserialize;
use std::collections::HashMap;
use egui::{Color32, Visuals, style::{Widgets, WidgetVisuals, Selection}, Stroke};

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ZedThemeFile {
    pub name: String,
    pub themes: Vec<ZedTheme>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ZedTheme {
    pub name: String,
    pub appearance: String,
    pub style: ZedStyle,
}

#[derive(Debug, Deserialize)]
pub struct ZedStyle {
    pub colors: HashMap<String, String>,
}

pub fn parse_hex_color(hex: &str) -> Color32 {
    let hex = hex.trim_start_matches('#');
    if hex.len() == 6 {
        let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
        let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
        let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);
        Color32::from_rgb(r, g, b)
    } else if hex.len() == 8 {
        let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
        let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
        let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);
        let a = u8::from_str_radix(&hex[6..8], 16).unwrap_or(255);
        Color32::from_rgba_unmultiplied(r, g, b, a)
    } else {
        Color32::GRAY
    }
}

pub fn map_zed_to_egui(zed: &ZedTheme) -> Visuals {
    let colors = &zed.style.colors;

    let bg = colors.get("background").map(|s| parse_hex_color(s)).unwrap_or(Color32::from_rgb(30, 30, 30));
    let surface = colors.get("surface").map(|s| parse_hex_color(s)).unwrap_or(Color32::from_rgb(45, 45, 45));
    let text = colors.get("text").map(|s| parse_hex_color(s)).unwrap_or(Color32::WHITE);
    let border = colors.get("border").map(|s| parse_hex_color(s)).unwrap_or(Color32::from_gray(60));
    let accent = colors.get("element.active").map(|s| parse_hex_color(s)).unwrap_or(Color32::BLUE);

    let mut visuals = if zed.appearance == "dark" {
        Visuals::dark()
    } else {
        Visuals::light()
    };

    visuals.window_fill = bg;
    visuals.panel_fill = bg;
    visuals.extreme_bg_color = surface;

    let widget_visuals = WidgetVisuals {
        bg_fill: surface,
        weak_bg_fill: surface,
        bg_stroke: Stroke::new(1.0, border),
        fg_stroke: Stroke::new(1.0, text),
        expansion: 0.0,
        corner_radius: egui::CornerRadius::same(4),
    };

    visuals.widgets = Widgets {
        noninteractive: widget_visuals,
        inactive: widget_visuals,
        hovered: WidgetVisuals {
            bg_fill: surface,
            bg_stroke: Stroke::new(1.0, accent),
            ..widget_visuals
        },
        active: WidgetVisuals {
            bg_fill: accent,
            fg_stroke: Stroke::new(1.0, bg),
            ..widget_visuals
        },
        open: widget_visuals,
    };

    visuals.selection = Selection {
        bg_fill: accent,
        stroke: Stroke::new(1.0, text),
    };

    visuals.window_corner_radius = egui::CornerRadius::same(8);
    visuals.window_stroke = Stroke::new(1.0, border);

    visuals
}

pub fn load_theme(theme_name: &str, config_path: &std::path::Path) -> Option<Visuals> {
    // If config_path is like .config/mhd/config.toml, we should look in .config/mhd/themes/
    let parent = config_path.parent().unwrap_or(std::path::Path::new(""));
    let theme_path = parent.join("themes").join(format!("{}.json", theme_name));
    
    // Fallback to local themes dir if it doesn't exist next to config
    let theme_path = if theme_path.exists() {
        theme_path
    } else {
        std::path::PathBuf::from("themes").join(format!("{}.json", theme_name))
    };

    if let Ok(content) = std::fs::read_to_string(theme_path) {
        if let Ok(zed_file) = serde_json::from_str::<ZedThemeFile>(&content) {
            if let Some(zed_theme) = zed_file.themes.first() {
                return Some(map_zed_to_egui(zed_theme));
            }
        }
    }
    None
}

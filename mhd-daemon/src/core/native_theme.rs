//! Shared JSON theme loader for native Win32 UI components.
//!
//! Reads mhd JSON theme files from
//! `%USERPROFILE%\.config\mhd\themes\{name}.json`.
//!
//! If the file is missing, malformed, or no theme is configured the
//! built‑in dark fallback is used.  The daemon never crashes because of
//! a missing/broken theme file.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;
use windows::Win32::Foundation::COLORREF;

// -----------------------------------------------------------------------
// Colour type
// -----------------------------------------------------------------------

/// 32‑bit sRGB colour with alpha channel.
///
/// All methods use **non‑premultiplied** RGB storage; premultiplication
//  is applied only at the final conversion step for layered DIB pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Argb {
    pub a: u8,
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Argb {
    pub const fn new(a: u8, r: u8, g: u8, b: u8) -> Self {
        Self { a, r, g, b }
    }

    /// Parse a hex colour string.
    ///
    /// Supported formats:
    /// - `#RRGGBB`       → alpha = 255
    /// - `#RRGGBBAA`     → alpha from last byte
    /// - `RRGGBB` / `RRGGBBAA` (no `#`)
    ///
    /// Leading/trailing whitespace is ignored.
    pub fn from_hex(s: &str) -> Option<Self> {
        let s = s.trim().trim_start_matches('#');
        if s.len() != 6 && s.len() != 8 {
            return None;
        }
        let val = u32::from_str_radix(s, 16).ok()?;
        if s.len() == 6 {
            Some(Self {
                a: 0xFF,
                r: ((val >> 16) & 0xFF) as u8,
                g: ((val >> 8) & 0xFF) as u8,
                b: (val & 0xFF) as u8,
            })
        } else {
            Some(Self {
                r: ((val >> 24) & 0xFF) as u8,
                g: ((val >> 16) & 0xFF) as u8,
                b: ((val >> 8) & 0xFF) as u8,
                a: (val & 0xFF) as u8,
            })
        }
    }

    /// Convert to GDI `COLORREF`.  GDI uses `0x00BBGGRR`.
    pub fn to_colorref(self) -> COLORREF {
        COLORREF(((self.b as u32) << 16) | ((self.g as u32) << 8) | self.r as u32)
    }

    /// Convert to a premultiplied ARGB pixel value suitable for
    /// `UpdateLayeredWindow` with `AC_SRC_ALPHA`.
    ///
    /// Returns `0xAARRGGBB` with RGB channels multiplied by alpha/255.
    pub fn to_premultiplied_argb_pixel(self) -> u32 {
        let a = self.a as u32;
        let r = (self.r as u32 * a + 127) / 255;
        let g = (self.g as u32 * a + 127) / 255;
        let b = (self.b as u32 * a + 127) / 255;
        (a << 24) | (r << 16) | (g << 8) | b
    }

    /// Blend this color over a background color.
    ///
    /// Returns an opaque color (alpha = 255) that looks like this color
    /// when drawn over the given background.
    /// Return black or white text colour that has sufficient contrast
    /// against this background (based on WCAG‑style luminance).
    /// Return a copy with the alpha channel replaced.
    pub fn with_alpha(&self, a: u8) -> Argb {
        Argb {
            a,
            r: self.r,
            g: self.g,
            b: self.b,
        }
    }

    pub fn contrasting_text_color(&self) -> Argb {
        let lum = 0.2126 * (self.r as f32 / 255.0)
            + 0.7152 * (self.g as f32 / 255.0)
            + 0.0722 * (self.b as f32 / 255.0);
        if lum < 0.5 {
            Argb::new(255, 255, 255, 255) // white
        } else {
            Argb::new(255, 0, 0, 0) // black
        }
    }

    pub fn blend_over(self, background: Argb) -> Argb {
        if self.a == 255 {
            return Argb { a: 255, ..self };
        }
        if self.a == 0 {
            return Argb {
                a: 255,
                ..background
            };
        }

        let a = self.a as u32;
        let inv_a = 255 - a;

        let r = (self.r as u32 * a + background.r as u32 * inv_a) / 255;
        let g = (self.g as u32 * a + background.g as u32 * inv_a) / 255;
        let b = (self.b as u32 * a + background.b as u32 * inv_a) / 255;

        Argb {
            a: 255,
            r: r as u8,
            g: g as u8,
            b: b as u8,
        }
    }
}

// -----------------------------------------------------------------------
// Theme data
// -----------------------------------------------------------------------

/// All colour values that native Win32 components consume.
#[derive(Debug, Clone)]
pub struct NativeTheme {
    pub name: String,

    // Base background colour (used for layered windows / popup background)
    pub background: Argb,
    // Surface / panel colour (edit control background, etc.)
    pub surface: Argb,
    // Separator / border colour
    pub border: Argb,

    // Primary text colour
    pub text: Argb,
    // Muted / secondary text (version, path, hints)
    pub text_muted: Argb,

    // Accent colour (progress bar fill, active elements)
    pub accent: Argb,
    // Selected state background
    pub selected: Argb,
    // Hover state background
    pub hover: Argb,

    // Progress bar track colour
    pub bar_background: Argb,
}

impl Default for NativeTheme {
    fn default() -> Self {
        Self {
            name: "built-in dark".into(),
            background: Argb::new(0xDD, 0x20, 0x20, 0x20),
            surface: Argb::new(0xFF, 0x2A, 0x2A, 0x2A),
            border: Argb::new(0xFF, 0x33, 0x33, 0x33),
            text: Argb::new(0xFF, 0xFF, 0xFF, 0xFF),
            text_muted: Argb::new(0xFF, 0x99, 0x99, 0x99),
            accent: Argb::new(0xFF, 0xFF, 0x8C, 0x00),
            selected: Argb::new(0x22, 0xFF, 0xFF, 0xFF),
            hover: Argb::new(0x22, 0xFF, 0xFF, 0xFF),
            bar_background: Argb::new(0xFF, 0x50, 0x50, 0x50),
        }
    }
}

// -----------------------------------------------------------------------
// Deserialisation support
// -----------------------------------------------------------------------

/// Top‑level theme file.
#[derive(Deserialize)]
struct ThemeFile {
    #[allow(dead_code)]
    name: Option<String>,
    #[allow(dead_code)]
    author: Option<String>,
    themes: Vec<ThemeEntry>,
}

#[derive(Deserialize)]
struct ThemeEntry {
    #[allow(dead_code)]
    name: Option<String>,
    #[allow(dead_code)]
    appearance: Option<String>,
    style: ThemeStyle,
}

#[derive(Deserialize)]
struct ThemeStyle {
    colors: HashMap<String, String>,
}

/// Colours parsed from a JSON theme entry.
struct ThemeColors {
    background: Option<Argb>,
    surface: Option<Argb>,
    border: Option<Argb>,
    text: Option<Argb>,
    text_muted: Option<Argb>,
    accent: Option<Argb>,
    selected: Option<Argb>,
    hover: Option<Argb>,
}

const DEFAULT_COLORS: ThemeColors = ThemeColors {
    background: None,
    surface: None,
    border: None,
    text: None,
    text_muted: None,
    accent: None,
    selected: None,
    hover: None,
};

impl ThemeColors {
    fn from_map(colors: &HashMap<String, String>) -> Self {
        let mut tc = ThemeColors { ..DEFAULT_COLORS };
        for (k, v) in colors {
            let argb = Argb::from_hex(v);
            match k.as_str() {
                "background" => tc.background = argb,
                "surface" => tc.surface = argb,
                "border" => tc.border = argb,
                "text" => tc.text = argb,
                "text.muted" => tc.text_muted = argb,
                "element.active" => tc.accent = argb,
                "element.selected" => tc.selected = argb,
                "element.hover" => tc.hover = argb,
                _ => {}
            }
        }
        tc
    }
}

// -----------------------------------------------------------------------
// Directory resolution
// -----------------------------------------------------------------------

/// Return the themes directory.
///
/// Resolution order:
/// 1. `$USERPROFILE\.config\mhd\themes`
/// 2. If `$USERPROFILE` is unset: `\.config\mhd\themes` (current drive root fallback)
pub fn themes_dir() -> PathBuf {
    if let Ok(home) = std::env::var("USERPROFILE") {
        PathBuf::from(home)
            .join(".config")
            .join("mhd")
            .join("themes")
    } else {
        PathBuf::from(r"\.config\mhd\themes")
    }
}

// -----------------------------------------------------------------------
// Public API
// -----------------------------------------------------------------------

/// Load a theme by name (without extension) from the standard themes
/// directory.  Silently falls back to [`NativeTheme::default`] on any
/// error (file missing, parse failure, etc.).
pub fn load_theme(theme_name: Option<&str>) -> NativeTheme {
    let name = match theme_name {
        Some(n) if !n.is_empty() => n,
        _ => return NativeTheme::default(),
    };

    let dir = themes_dir();
    let path = dir.join(format!("{name}.json"));
    load_theme_from_path(&path).unwrap_or_else(|_| NativeTheme::default())
}

/// Load a theme from an explicit file path.  Returns an `Err` if the
/// file cannot be read or is syntactically/structurally invalid.
pub fn load_theme_from_path(path: &std::path::Path) -> Result<NativeTheme, String> {
    let json = std::fs::read_to_string(path).map_err(|e| format!("cannot read theme file: {e}"))?;
    let file: ThemeFile =
        serde_json::from_str(&json).map_err(|e| format!("JSON parse error: {e}"))?;

    let entry = file
        .themes
        .first()
        .ok_or_else(|| "no theme entries found".to_string())?;

    let tc = ThemeColors::from_map(&entry.style.colors);

    let def = NativeTheme::default();
    Ok(NativeTheme {
        name: entry.name.clone().unwrap_or_else(|| "unknown".into()),
        background: tc.background.unwrap_or(def.background),
        surface: tc.surface.unwrap_or(def.surface),
        border: tc.border.unwrap_or(def.border),
        text: tc.text.unwrap_or(def.text),
        text_muted: tc.text_muted.unwrap_or(def.text_muted),
        accent: tc.accent.unwrap_or(def.accent),
        selected: tc.selected.unwrap_or(def.selected),
        hover: tc.hover.unwrap_or(def.hover),
        bar_background: def.bar_background,
    })
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rgb_hex() {
        let c = Argb::from_hex("#FF8C00").unwrap();
        assert_eq!(c, Argb::new(0xFF, 0xFF, 0x8C, 0x00));
    }

    #[test]
    fn parse_rgba_hex() {
        let c = Argb::from_hex("#202020DD").unwrap();
        assert_eq!(c, Argb::new(0xDD, 0x20, 0x20, 0x20));
    }

    #[test]
    fn parse_no_hash() {
        let c = Argb::from_hex("FFFFFF").unwrap();
        assert_eq!(c, Argb::new(0xFF, 0xFF, 0xFF, 0xFF));
    }

    #[test]
    fn parse_invalid_returns_none() {
        assert!(Argb::from_hex("#GGG").is_none());
        assert!(Argb::from_hex("").is_none());
    }

    #[test]
    fn colorref_is_bgr() {
        let c = Argb::new(0xFF, 0xFF, 0x8C, 0x00);
        assert_eq!(c.to_colorref(), COLORREF(0x00008CFF)); // 0x00BBGGRR
    }

    #[test]
    fn premultiplied_fully_opaque() {
        let c = Argb::new(0xFF, 0xFF, 0x8C, 0x00);
        let px = c.to_premultiplied_argb_pixel();
        // a=0xFF, r=0xFF, g=0x8C, b=0x00 → 0xFFFF8C00
        assert_eq!(px, 0xFFFF8C00);
    }

    #[test]
    fn premultiplied_partial_alpha() {
        let c = Argb::new(0x80, 0xFF, 0xFF, 0xFF);
        let px = c.to_premultiplied_argb_pixel();
        // a=0x80, r=0xFF*0x80/255=0x80, g=0x80, b=0x80 → 0x80808080
        assert_eq!(px, 0x80808080);
    }

    #[test]
    fn load_default_theme_when_none() {
        let t = load_theme(None);
        assert_eq!(t.name, "built-in dark");
    }

    #[test]
    fn load_default_theme_when_bad_name() {
        let t = load_theme(Some("nonexistent_theme_xyz"));
        assert_eq!(t.name, "built-in dark");
    }
}

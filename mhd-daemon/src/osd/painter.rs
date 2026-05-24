use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, DrawTextW, FillRect,
    SelectObject, SetBkMode, SetTextColor,
    DT_END_ELLIPSIS, DT_LEFT, DT_SINGLELINE, DT_VCENTER,
    TRANSPARENT,
};

use crate::native_theme::NativeTheme;
use crate::renderer::{DibFrame, to_utf16_z, create_font, draw_rounded_rect};

pub fn paint_osd(
    hwnd: HWND,
    value: u32,
    monitor_name: &str,
    work: &RECT,
    width: i32,
    height: i32,
    scale: f32,
    theme: &NativeTheme,
) {
    let mut frame = match DibFrame::new(width, height) {
        Some(f) => f,
        None => return,
    };

    let radius = (14.0 * scale) as i32;
    draw_rounded_rect(frame.pixels_mut(), width, height, radius, theme.background);

    let font_h = -(14.0 * scale) as i32;
    let font_small_h = -(11.0 * scale) as i32;

    let hfont = create_font(font_h, false, "Segoe UI");
    let hfont_small = create_font(font_small_h, false, "Segoe UI");

    let old_font = unsafe { SelectObject(frame.dc(), hfont) };

    unsafe {
        let _ = SetBkMode(frame.dc(), TRANSPARENT);
        let _ = SetTextColor(frame.dc(), theme.text.to_colorref());
    }

    let pad = (20.0 * scale) as i32;

    // Monitor name
    let name_y = pad + 4;
    let mut name_rc = RECT {
        left: pad + radius / 2,
        top: name_y,
        right: width - pad,
        bottom: name_y + font_h.abs() * 3 / 2,
    };
    let mut name_wz = to_utf16_z(monitor_name);
    unsafe {
        let _ = DrawTextW(
            frame.dc(),
            &mut name_wz,
            &mut name_rc,
            DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
    }

    // "Brightness" label
    unsafe {
        let _ = SetTextColor(frame.dc(), theme.text_muted.to_colorref());
    }
    let lbl_y = name_y + font_h.abs() + 12;
    let mut lbl_rc = RECT {
        left: pad + radius / 2,
        top: lbl_y,
        right: width - pad,
        bottom: lbl_y + font_small_h.abs() * 3 / 2 + 4,
    };
    let mut label_wide = to_utf16_z("Brightness");
    unsafe {
        let _ = DrawTextW(frame.dc(), &mut label_wide, &mut lbl_rc, DT_LEFT | DT_SINGLELINE);
    }
    unsafe {
        let _ = SetTextColor(frame.dc(), theme.text.to_colorref());
    }

    // Progress bar
    let bar_y = lbl_y + font_small_h.abs() + 12;
    let bar_w = ((width - pad * 2) as f32 * 0.78) as i32;
    let bar_x = pad + radius / 2;
    let bar_h = ((6.0 * scale) as i32).max(2);

    // Track
    {
        let track_brush = unsafe { CreateSolidBrush(theme.bar_background.to_colorref()) };
        let track_rc = RECT {
            left: bar_x,
            top: bar_y,
            right: bar_x + bar_w,
            bottom: bar_y + bar_h,
        };
        unsafe {
            let _ = FillRect(frame.dc(), &track_rc, track_brush);
            let _ = DeleteObject(track_brush);
        }
    }

    // Fill
    let fill_w = ((bar_w as f32) * (value.min(100) as f32 / 100.0)) as i32;
    if fill_w > 0 {
        let accent = unsafe { CreateSolidBrush(theme.accent.to_colorref()) };
        let fill_rc = RECT {
            left: bar_x,
            top: bar_y,
            right: bar_x + fill_w,
            bottom: bar_y + bar_h,
        };
        unsafe {
            let _ = FillRect(frame.dc(), &fill_rc, accent);
            let _ = DeleteObject(accent);
        }
    }

    // Percentage label
    let pct = format!("{}%", value.min(100));
    let mut pct_wz = to_utf16_z(&pct);
    let mut pct_rc = RECT {
        left: bar_x + bar_w + 12,
        top: bar_y - 4,
        right: width - pad,
        bottom: bar_y + bar_h + 4,
    };
    unsafe {
        let _ = DrawTextW(
            frame.dc(),
            &mut pct_wz,
            &mut pct_rc,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
    }

    unsafe {
        let _ = SelectObject(frame.dc(), old_font);
        let _ = DeleteObject(hfont);
        let _ = DeleteObject(hfont_small);
    }

    frame.fix_gdi_alpha(theme.background);

    let pos_x = work.left + (work.right - work.left - width) / 2;
    let pos_y = work.top + (work.bottom - work.top - height) / 2;
    frame.present_layered(hwnd, pos_x, pos_y, 255);
}



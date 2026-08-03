//! Rendering and paint-time hit geometry for the CPU power-plan overlay.

use super::*;

// ── Painting ─────────────────────────────────────────────────────────

pub(super) fn paint_panel(hwnd: HWND, st: &PanelState, w: i32, h: i32, sc: f32) {
    let mut frame = match crate::renderer::DibFrame::new(w, h) {
        Some(f) => f,
        None => return,
    };
    let pixels = frame.pixels_mut();
    crate::osd::draw_rounded_rect(
        pixels,
        w,
        h,
        (RADIUS as f32 * sc) as i32,
        st.theme.background,
    );
    let mem = frame.dc();
    let font = crate::osd::create_font(-(14.0 * sc) as i32, false, "Segoe UI");
    let _of = unsafe { SelectObject(mem, font) };

    let fg = st.theme.text;
    let accent = st.theme.accent;
    let dim = st.theme.text_muted;
    // let bg = st.theme.background;
    let border = st.theme.border;
    let pad_sc = (PAD as f32 * sc) as i32;
    let s = sc;

    let ranges = settings_y_ranges(sc);

    // ── Header ──
    tcol(mem, fg);
    dw(
        mem,
        &mut to_utf16_z("CPU Power Plan"),
        &mut rct(0, (8.0 * s) as i32, w, (HDR_H as f32 * s) as i32),
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );

    // Close ✕
    let cx = w - pad_sc - (20.0 * s) as i32;
    let cx2 = cx + (20.0 * s) as i32;
    tcol(mem, dim);
    dw(
        mem,
        &mut to_utf16_z("✕"),
        &mut rct(
            cx,
            (8.0 * s) as i32,
            cx2,
            (HDR_H as f32 * s) as i32 - (8.0 * s) as i32,
        ),
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );

    // ── Active plan name (clickable) ──
    let plan_y = (HDR_H as f32 * s) as i32;
    tcol(mem, accent);
    let mut plan_text = to_utf16_z(&format!("▸ {}", st.active_plan_name));
    dw(
        mem,
        &mut plan_text,
        &mut rct(
            pad_sc,
            plan_y,
            w - pad_sc,
            plan_y + (PLAN_H as f32 * s) as i32,
        ),
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );

    // ── Separator after plan row ──
    let sep1_y = plan_y + (PLAN_H as f32 * s) as i32;
    fill_rect(mem, pad_sc, sep1_y, w - pad_sc, sep1_y + 1, border);

    // ── Settings section ──────────────────────────────────────────────

    let sx = pad_sc + (LABEL_W as f32 * s) as i32;
    let vw = (VAL_W as f32 * s) as i32;
    let gap = (PAD_INNER as f32 * s) as i32;

    // Column headers for AC/DC
    tcol(mem, dim);
    dw(
        mem,
        &mut to_utf16_z("AC"),
        &mut rct(
            sx,
            ranges.cores_sec_y,
            sx + vw,
            ranges.cores_sec_y + (SEC_H as f32 * s) as i32,
        ),
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );
    dw(
        mem,
        &mut to_utf16_z("DC"),
        &mut rct(
            sx + vw + gap,
            ranges.cores_sec_y,
            sx + vw + gap + vw,
            ranges.cores_sec_y + (SEC_H as f32 * s) as i32,
        ),
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );

    // ── CORES section ──
    draw_section_header(mem, pad_sc, ranges.cores_sec_y, w, s, "CORES", fg, dim);
    let row_h = (ROW_H as f32 * s) as i32;

    // Min Cores %
    let ry = ranges.min_cores_y;
    tcol(mem, fg);
    dw(
        mem,
        &mut to_utf16_z("Min Cores %"),
        &mut rct(pad_sc, ry, pad_sc + (LABEL_W as f32 * s) as i32, ry + row_h),
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );
    let ac_str = if st.focused == Some(Field::AcMinCores) {
        edit_display(st)
    } else {
        format!("{}%", st.ac.min_cores)
    };
    draw_value_cell(
        mem,
        sx,
        ry,
        vw,
        row_h,
        s,
        &ac_str,
        Field::AcMinCores,
        st.focused,
        st.dirty,
        fg,
        dim,
        accent,
    );
    let dc_str = if st.focused == Some(Field::DcMinCores) {
        edit_display(st)
    } else {
        format!("{}%", st.dc.min_cores)
    };
    draw_value_cell(
        mem,
        sx + vw + gap,
        ry,
        vw,
        row_h,
        s,
        &dc_str,
        Field::DcMinCores,
        st.focused,
        st.dirty,
        fg,
        dim,
        accent,
    );

    // Max Cores %
    let ry = ranges.max_cores_y;
    tcol(mem, fg);
    dw(
        mem,
        &mut to_utf16_z("Max Cores %"),
        &mut rct(pad_sc, ry, pad_sc + (LABEL_W as f32 * s) as i32, ry + row_h),
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );
    let ac_str = if st.focused == Some(Field::AcMaxCores) {
        edit_display(st)
    } else {
        format!("{}%", st.ac.max_cores)
    };
    draw_value_cell(
        mem,
        sx,
        ry,
        vw,
        row_h,
        s,
        &ac_str,
        Field::AcMaxCores,
        st.focused,
        st.dirty,
        fg,
        dim,
        accent,
    );
    let dc_str = if st.focused == Some(Field::DcMaxCores) {
        edit_display(st)
    } else {
        format!("{}%", st.dc.max_cores)
    };
    draw_value_cell(
        mem,
        sx + vw + gap,
        ry,
        vw,
        row_h,
        s,
        &dc_str,
        Field::DcMaxCores,
        st.focused,
        st.dirty,
        fg,
        dim,
        accent,
    );

    // ── FREQUENCY section ──
    draw_section_header(mem, pad_sc, ranges.freq_sec_y, w, s, "FREQUENCY", fg, dim);

    // Min Speed %
    let ry_min = ranges.min_freq_y;
    tcol(mem, fg);
    dw(
        mem,
        &mut to_utf16_z("Min Speed %"),
        &mut rct(
            pad_sc,
            ry_min,
            pad_sc + (LABEL_W as f32 * s) as i32,
            ry_min + row_h,
        ),
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );
    let ac_str = if st.focused == Some(Field::AcMinFreq) {
        edit_display(st)
    } else {
        format!("{}%", st.ac.min_freq)
    };
    draw_value_cell(
        mem,
        sx,
        ry_min,
        vw,
        row_h,
        s,
        &ac_str,
        Field::AcMinFreq,
        st.focused,
        st.dirty,
        fg,
        dim,
        accent,
    );
    let dc_str = if st.focused == Some(Field::DcMinFreq) {
        edit_display(st)
    } else {
        format!("{}%", st.dc.min_freq)
    };
    draw_value_cell(
        mem,
        sx + vw + gap,
        ry_min,
        vw,
        row_h,
        s,
        &dc_str,
        Field::DcMinFreq,
        st.focused,
        st.dirty,
        fg,
        dim,
        accent,
    );

    // Max Speed %
    let ry_max = ranges.max_freq_y;
    tcol(mem, fg);
    dw(
        mem,
        &mut to_utf16_z("Max Speed %"),
        &mut rct(
            pad_sc,
            ry_max,
            pad_sc + (LABEL_W as f32 * s) as i32,
            ry_max + row_h,
        ),
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );
    let ac_str = if st.focused == Some(Field::AcMaxFreq) {
        edit_display(st)
    } else {
        format!("{}%", st.ac.max_freq)
    };
    draw_value_cell(
        mem,
        sx,
        ry_max,
        vw,
        row_h,
        s,
        &ac_str,
        Field::AcMaxFreq,
        st.focused,
        st.dirty,
        fg,
        dim,
        accent,
    );
    let dc_str = if st.focused == Some(Field::DcMaxFreq) {
        edit_display(st)
    } else {
        format!("{}%", st.dc.max_freq)
    };
    draw_value_cell(
        mem,
        sx + vw + gap,
        ry_max,
        vw,
        row_h,
        s,
        &dc_str,
        Field::DcMaxFreq,
        st.focused,
        st.dirty,
        fg,
        dim,
        accent,
    );

    // ── POWER FEATURES section ──
    draw_section_header(
        mem,
        pad_sc,
        ranges.pf_sec_y,
        w,
        s,
        "POWER FEATURES",
        fg,
        dim,
    );

    // Autonomous Mode toggle
    let ry_am = ranges.autonomous_mode_y;
    draw_settings_label(
        mem,
        pad_sc,
        ry_am,
        row_h,
        s,
        "Autonomous Mode",
        st.hover_row == Some(HoverRow::AutonomousMode),
        fg,
        accent,
    );
    draw_toggle(
        mem,
        sx,
        ry_am,
        vw,
        row_h,
        s,
        st.ac.autonomous_mode,
        &st.theme,
    );
    draw_toggle(
        mem,
        sx + vw + gap,
        ry_am,
        vw,
        row_h,
        s,
        st.dc.autonomous_mode,
        &st.theme,
    );

    // Turbo Boost toggle
    let ry_tb = ranges.turbo_y;
    draw_settings_label(
        mem,
        pad_sc,
        ry_tb,
        row_h,
        s,
        "Turbo Boost",
        st.hover_row == Some(HoverRow::TurboBoost),
        fg,
        accent,
    );
    draw_toggle(mem, sx, ry_tb, vw, row_h, s, st.ac.turbo, &st.theme);
    draw_toggle(
        mem,
        sx + vw + gap,
        ry_tb,
        vw,
        row_h,
        s,
        st.dc.turbo,
        &st.theme,
    );

    // Cooling Policy dropdown
    let ry_cp = ranges.cooling_policy_y;
    draw_settings_label(
        mem,
        pad_sc,
        ry_cp,
        row_h,
        s,
        "Cooling Policy",
        st.hover_row == Some(HoverRow::CoolingPolicy),
        fg,
        accent,
    );
    draw_dropdown_cell(
        mem,
        sx,
        ry_cp,
        vw,
        row_h,
        s,
        cooling_policy_label(st.ac.cooling_policy),
        &st.theme,
    );
    draw_dropdown_cell(
        mem,
        sx + vw + gap,
        ry_cp,
        vw,
        row_h,
        s,
        cooling_policy_label(st.dc.cooling_policy),
        &st.theme,
    );

    // Increase Policy dropdown
    let ry_ip = ranges.increase_policy_y;
    draw_settings_label(
        mem,
        pad_sc,
        ry_ip,
        row_h,
        s,
        "Increase Policy",
        st.hover_row == Some(HoverRow::IncreasePolicy),
        fg,
        accent,
    );
    draw_dropdown_cell(
        mem,
        sx,
        ry_ip,
        vw,
        row_h,
        s,
        increase_policy_label(st.ac.increase_policy),
        &st.theme,
    );
    draw_dropdown_cell(
        mem,
        sx + vw + gap,
        ry_ip,
        vw,
        row_h,
        s,
        increase_policy_label(st.dc.increase_policy),
        &st.theme,
    );

    // ── CORE MANAGEMENT section ──
    draw_section_header(
        mem,
        pad_sc,
        ranges.cm_sec_y,
        w,
        s,
        "CORE MANAGEMENT",
        fg,
        dim,
    );

    // Heterogeneous Scheduling dropdown
    let ry_hp = ranges.hetero_policy_y;
    draw_settings_label(
        mem,
        pad_sc,
        ry_hp,
        row_h,
        s,
        "Hetero Sched",
        st.hover_row == Some(HoverRow::HeteroScheduling),
        fg,
        accent,
    );
    draw_dropdown_cell(
        mem,
        sx,
        ry_hp,
        vw,
        row_h,
        s,
        hetero_policy_label(st.ac.hetero_policy),
        &st.theme,
    );
    draw_dropdown_cell(
        mem,
        sx + vw + gap,
        ry_hp,
        vw,
        row_h,
        s,
        hetero_policy_label(st.dc.hetero_policy),
        &st.theme,
    );

    // Parked Core Performance dropdown
    let ry_pp = ranges.parked_perf_y;
    draw_settings_label(
        mem,
        pad_sc,
        ry_pp,
        row_h,
        s,
        "Parked Perf",
        st.hover_row == Some(HoverRow::ParkedPerf),
        fg,
        accent,
    );
    draw_dropdown_cell(
        mem,
        sx,
        ry_pp,
        vw,
        row_h,
        s,
        parked_perf_label(st.ac.parked_perf),
        &st.theme,
    );
    draw_dropdown_cell(
        mem,
        sx + vw + gap,
        ry_pp,
        vw,
        row_h,
        s,
        parked_perf_label(st.dc.parked_perf),
        &st.theme,
    );

    // ── Save button (under the parameters; edits apply live, this commits) ──
    {
        let btn_w = (BTN_W as f32 * s) as i32;
        let btn_h_actual = (BTN_H as f32 * s) as i32;
        let btn_row_h = (BTN_ROW_H as f32 * s) as i32;
        let btn_y = ranges.save_row_y + (btn_row_h - btn_h_actual) / 2;
        let bx = (w - btn_w) / 2;
        // Highlight Save only when there are unsaved edits to commit.
        let btn_color = if st.dirty { accent } else { dim };
        fill_rect(mem, bx, btn_y, bx + btn_w, btn_y + btn_h_actual, btn_color);
        tcol(mem, btn_color.contrasting_text_color());
        dw(
            mem,
            &mut to_utf16_z("Save"),
            &mut rct(bx, btn_y, bx + btn_w, btn_y + btn_h_actual),
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
    }

    // ── Separator before monitor section ──
    let sep2_y = ranges.mon_hdr_y - (SEP_H as f32 * s) as i32;
    fill_rect(mem, pad_sc, sep2_y, w - pad_sc, sep2_y + 1, border);

    // ── LIVE MONITOR section ──
    let mon_y = ranges.mon_hdr_y;
    let mon_h = (MON_HDR_H as f32 * s) as i32;

    tcol(mem, fg);
    dw(
        mem,
        &mut to_utf16_z("LIVE MONITOR"),
        &mut rct(pad_sc, mon_y, (W as f32 * s / 2.0) as i32, mon_y + mon_h),
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );

    let load_y = mon_y + mon_h;
    let load_h = (LOAD_ROW_H as f32 * s) as i32;
    draw_stress_buttons(mem, w, load_y, load_h, s, &st.monitor, &st.theme);

    // ── Core bars ──
    let mut content_y = load_y + load_h;

    // Bar area width
    let bar_area_w = w - pad_sc * 2;

    // Shared frequency scale + Max-Processor-State ceiling (AC value, % of scale).
    let scale_mhz = st.monitor.freq_scale_mhz;
    let cap_mhz = (scale_mhz as u64 * st.ac.max_freq.min(100) as u64 / 100) as u32;

    // ── PACKAGE summary line ──
    {
        let active = st.monitor.core_parked.iter().filter(|&&p| !p).count();
        let total = st.monitor.core_parked.len();
        let avg_mhz = package_avg_freq_mhz(&st.monitor);
        let pkg = if st.monitor.base_mhz > 0 {
            format!(
                "PACKAGE  avg {:.1}G / base {:.1}G    active {}/{}",
                avg_mhz as f32 / 1000.0,
                st.monitor.base_mhz as f32 / 1000.0,
                active,
                total
            )
        } else {
            format!(
                "PACKAGE  avg {:.1}G    active {}/{}",
                avg_mhz as f32 / 1000.0,
                active,
                total
            )
        };
        tcol(mem, fg);
        let pkg_h = (SUMMARY_H as f32 * s) as i32;
        dw(
            mem,
            &mut to_utf16_z(&pkg),
            &mut rct(pad_sc, content_y, w - pad_sc, content_y + pkg_h),
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
        content_y += pkg_h;
    }

    // ── Scrollable core-list viewport ──
    // The groups below the PACKAGE line scroll within [lt, h]; clip so partial
    // rows don't bleed into the fixed area above or the window edge below.
    let lt = list_top(s);
    let _ = content_y; // content_y == lt here; groups are positioned via scroll
    unsafe {
        let _ = IntersectClipRect(mem, 0, lt, w, h);
    }
    let mut gy = lt - st.scroll;

    if !st.monitor.p_cores.is_empty() {
        gy = draw_core_group(
            mem,
            "P",
            &st.monitor.p_cores,
            pad_sc,
            gy,
            bar_area_w,
            w,
            s,
            scale_mhz,
            cap_mhz,
            st.p_collapsed,
            &st.monitor,
            &st.theme,
            fg,
            dim,
        );
    }
    if !st.monitor.e_cores.is_empty() {
        gy = draw_core_group(
            mem,
            "E",
            &st.monitor.e_cores,
            pad_sc,
            gy,
            bar_area_w,
            w,
            s,
            scale_mhz,
            cap_mhz,
            st.e_collapsed,
            &st.monitor,
            &st.theme,
            fg,
            dim,
        );
    }
    if st.monitor.p_cores.is_empty() && st.monitor.e_cores.is_empty() {
        tcol(mem, dim);
        dw(
            mem,
            &mut to_utf16_z("No cores detected"),
            &mut rct(pad_sc, gy, w - pad_sc, gy + (ROW_H as f32 * s) as i32),
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
    }
    unsafe {
        let _ = SelectClipRgn(mem, HRGN::default());
    }

    // ── Scrollbar indicator (only when the list overflows) ──
    let smax = scroll_max(s, st, h);
    if smax > 0 {
        let track_x = w - (3.0 * s) as i32;
        let track_top = lt;
        let track_h = h - lt;
        fill_rect(
            mem,
            track_x,
            track_top,
            track_x + (2.0 * s) as i32,
            track_top + track_h,
            st.theme.bar_background,
        );
        let content = (list_content_h(
            st.monitor.p_cores.len(),
            st.monitor.e_cores.len(),
            st.p_collapsed,
            st.e_collapsed,
        ) as f32
            * s) as i32;
        let thumb_h = ((track_h as i64 * track_h as i64) / content.max(1) as i64)
            .max((12.0 * s) as i64) as i32;
        let thumb_y =
            track_top + ((track_h - thumb_h) as i64 * st.scroll as i64 / smax as i64) as i32;
        fill_rect(
            mem,
            track_x,
            thumb_y,
            track_x + (2.0 * s) as i32,
            thumb_y + thumb_h,
            st.theme.text_muted,
        );
    }

    // Draw tooltip on top of everything else, but only after the cursor has
    // rested on the row for a short dwell delay (the 500ms monitor timer tick
    // triggers the repaint that makes it appear).
    if let (Some(hover_row), Some(since)) = (st.hover_row, st.hover_since)
        && since.elapsed() >= std::time::Duration::from_millis(350)
    {
        draw_tooltip(mem, hover_row, st.hover_pos, w, h, s, &st.theme);
    }

    frame.fix_gdi_alpha(st.theme.background);

    unsafe {
        let mut wr = RECT::default();
        let _ = GetWindowRect(hwnd, &mut wr);
        frame.present_layered(hwnd, wr.left, wr.top, 255);
    }

    unsafe {
        let _ = DeleteObject(font);
    }
}

// Average current frequency across non-parked cores (MHz). Falls back to all
// cores if every core happens to be parked.
fn package_avg_freq_mhz(mon: &MonitorState) -> u32 {
    let mut sum = 0u64;
    let mut count = 0u64;
    for (i, &f) in mon.core_freq_mhz.iter().enumerate() {
        let parked = mon.core_parked.get(i).copied().unwrap_or(false);
        if !parked && f > 0 {
            sum += f as u64;
            count += 1;
        }
    }
    if count > 0 { (sum / count) as u32 } else { 0 }
}

fn draw_section_header(
    mem: HDC,
    x: i32,
    y: i32,
    w: i32,
    _sc: f32,
    label: &str,
    fg: Argb,
    _dim: Argb,
) {
    let sec_h = (SEC_H as f32 * _sc) as i32;
    // Dim label for section header
    tcol(mem, fg);
    dw(
        mem,
        &mut to_utf16_z(label),
        &mut rct(x, y, w - x, y + sec_h),
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );
}

fn edit_display(st: &PanelState) -> String {
    if st.edit_text.is_empty() {
        "|".to_string()
    } else {
        format!("{}|", st.edit_text)
    }
}

fn draw_value_cell(
    mem: HDC,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    _sc: f32,
    text: &str,
    field: Field,
    focused: Option<Field>,
    _dirty: bool,
    fg: Argb,
    dim: Argb,
    accent: Argb,
) {
    let is_focused = focused == Some(field);
    let cell_border = if is_focused { accent } else { dim };

    // Background for focused cell
    if is_focused {
        fill_rect(mem, x, y, x + w, y + h, accent.with_alpha(0x30));
    }
    // Border
    let bw = (1.0 * _sc) as i32;
    fill_rect(mem, x, y, x + w, y + bw, cell_border);
    fill_rect(mem, x, y + h - bw, x + w, y + h, cell_border);
    fill_rect(mem, x, y, x + bw, y + h, cell_border);
    fill_rect(mem, x + w - bw, y, x + w, y + h, cell_border);

    // Text
    tcol(mem, fg);
    dw(
        mem,
        &mut to_utf16_z(text),
        &mut rct(x + 4, y, x + w - 4, y + h),
        DT_RIGHT | DT_SINGLELINE | DT_VCENTER,
    );
}

fn draw_toggle(
    mem: HDC,
    col_x: i32,
    row_y: i32,
    vw: i32,
    row_h: i32,
    sc: f32,
    enabled: bool,
    theme: &NativeTheme,
) {
    let tog_w = (36.0 * sc) as i32;
    let tog_h = (16.0 * sc) as i32;
    let tx = col_x + (vw - tog_w) / 2;
    let ty = row_y + (row_h - tog_h) / 2;

    let pill = if enabled {
        theme.accent
    } else {
        theme.text_muted
    };
    fill_rect(mem, tx, ty, tx + tog_w, ty + tog_h, pill);

    // Knob
    let knob_m = (3.0 * sc) as i32;
    let knob_d = tog_h - knob_m * 2;
    let knob_x = if enabled {
        tx + tog_w - knob_d - knob_m
    } else {
        tx + knob_m
    };
    fill_rect(
        mem,
        knob_x,
        ty + knob_m,
        knob_x + knob_d,
        ty + knob_m + knob_d,
        theme.text,
    );
}

// Draw a settings row label; if hovered show it in accent color.
fn draw_settings_label(
    mem: HDC,
    pad_sc: i32,
    ry: i32,
    row_h: i32,
    sc: f32,
    label: &str,
    hovered: bool,
    fg: Argb,
    accent: Argb,
) {
    let label_w = (LABEL_W as f32 * sc) as i32;
    tcol(mem, if hovered { accent } else { fg });
    dw(
        mem,
        &mut to_utf16_z(label),
        &mut rct(pad_sc, ry, pad_sc + label_w, ry + row_h),
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );
}

// Draw a cycling dropdown cell (shows current value with ▼ indicator).
fn draw_dropdown_cell(
    mem: HDC,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    sc: f32,
    label: &str,
    theme: &NativeTheme,
) {
    let bw = (1.0 * sc) as i32;
    let border = theme.text_muted;
    fill_rect(mem, x, y, x + w, y + bw, border);
    fill_rect(mem, x, y + h - bw, x + w, y + h, border);
    fill_rect(mem, x, y, x + bw, y + h, border);
    fill_rect(mem, x + w - bw, y, x + w, y + h, border);
    tcol(mem, theme.text);
    let text = format!("{}▾", label);
    dw(
        mem,
        &mut to_utf16_z(&text),
        &mut rct(x + 2, y, x + w - 2, y + h),
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );
}

fn cooling_policy_label(v: u32) -> &'static str {
    if v == 0 { "Passive" } else { "Active" }
}
fn increase_policy_label(v: u32) -> &'static str {
    if v == 0 { "Ideal" } else { "Rocket" }
}
fn hetero_policy_label(v: u32) -> &'static str {
    match v {
        2 => "Prefer Perf",
        4 => "Prefer Eff",
        5 => "Auto",
        _ => "All cores",
    }
}
fn parked_perf_label(v: u32) -> &'static str {
    match v {
        1 => "Deepest",
        2 => "Lightest",
        _ => "No Pref",
    }
}

pub(super) fn hit_hover_row(x: i32, y: i32, sc: f32) -> Option<HoverRow> {
    let ranges = settings_y_ranges(sc);
    let row_h = (ROW_H as f32 * sc) as i32;
    let win_w = (W as f32 * sc) as i32;
    if x < 0 || x >= win_w {
        return None;
    }

    if hit_plan_row(y, sc) {
        return Some(HoverRow::PlanRow);
    }

    macro_rules! chk {
        ($ry:expr, $row:expr) => {
            if y >= $ry && y < $ry + row_h {
                return Some($row);
            }
        };
    }
    chk!(ranges.min_cores_y, HoverRow::MinCores);
    chk!(ranges.max_cores_y, HoverRow::MaxCores);
    chk!(ranges.min_freq_y, HoverRow::MinFreq);
    chk!(ranges.max_freq_y, HoverRow::MaxFreq);
    chk!(ranges.autonomous_mode_y, HoverRow::AutonomousMode);
    chk!(ranges.turbo_y, HoverRow::TurboBoost);
    chk!(ranges.cooling_policy_y, HoverRow::CoolingPolicy);
    chk!(ranges.increase_policy_y, HoverRow::IncreasePolicy);
    chk!(ranges.hetero_policy_y, HoverRow::HeteroScheduling);
    chk!(ranges.parked_perf_y, HoverRow::ParkedPerf);
    None
}

fn tooltip_lines(row: HoverRow) -> &'static [&'static str] {
    match row {
        HoverRow::PlanRow => &[
            "Active Power Plan",
            "Click to cycle through the",
            "available Windows power plans.",
        ],
        HoverRow::MinCores => &[
            "Minimum Processor Cores",
            "Lowest % of cores kept unparked.",
            "Lower: more parking, less power.",
            "Left = AC (plugged), right = DC.",
        ],
        HoverRow::MaxCores => &[
            "Maximum Processor Cores",
            "Highest % of cores allowed active.",
            "Lower: caps parallelism / heat.",
            "Left = AC (plugged), right = DC.",
        ],
        HoverRow::MinFreq => &[
            "Minimum Processor State",
            "Lowest CPU frequency, as % of max.",
            "Lower: cooler/quieter idle.",
            "Left = AC (plugged), right = DC.",
        ],
        HoverRow::MaxFreq => &[
            "Maximum Processor State",
            "Highest CPU frequency, as % of max.",
            "Lower: caps clocks, heat & noise.",
            "Left = AC (plugged), right = DC.",
        ],
        HoverRow::AutonomousMode => &[
            "Autonomous Mode",
            "OFF: OS manages CPU frequencies",
            "ON:  CPU self-manages (faster)",
        ],
        HoverRow::TurboBoost => &[
            "Turbo / Boost",
            "OFF: CPU stays at base clock",
            "ON:  Allows burst above base",
        ],
        HoverRow::CoolingPolicy => &[
            "System Cooling Policy",
            "Passive: Lower freq instead of fans",
            "  -> Quieter, slightly slower",
            "Active:  Keep freq, spin fans",
            "  -> Faster, noisier",
        ],
        HoverRow::IncreasePolicy => &[
            "Performance Increase Policy",
            "Ideal:  Gradual boost (cooler/quiet)",
            "Rocket: Instant max (responsive)",
        ],
        HoverRow::HeteroScheduling => &[
            "Heterogeneous Thread Scheduling",
            "All (0):        Any core",
            "Perf first (2): P-cores priority",
            "Eff first (4):  E-cores (quiet)",
            "Auto (5):       System decides",
        ],
        HoverRow::ParkedPerf => &[
            "Parked Core Performance State",
            "No Pref (0): Default behavior",
            "Deepest (1): Max power savings",
            "Lightest (2): Quick wake-up",
        ],
    }
}

// Draw tooltip for the hovered row (floats over panel at cursor position).
fn draw_tooltip(
    mem: HDC,
    hover_row: HoverRow,
    hover_pos: POINT,
    win_w: i32,
    win_h: i32,
    sc: f32,
    theme: &NativeTheme,
) {
    let lines = tooltip_lines(hover_row);
    let line_h = (16.0 * sc) as i32;
    let pad = (6.0 * sc) as i32;
    let tip_w = (200.0 * sc) as i32;
    let tip_h = line_h * lines.len() as i32 + pad * 2;

    // Position: right of cursor, fully clamped inside the window so no edge
    // clips it (the tooltip lives in the panel's own layered surface).
    let mut tx = hover_pos.x + (10.0 * sc) as i32;
    let mut ty = hover_pos.y - tip_h / 2;
    if tx + tip_w > win_w {
        tx = hover_pos.x - tip_w - (6.0 * sc) as i32;
    }
    if tx + tip_w > win_w {
        tx = win_w - tip_w;
    }
    if tx < 0 {
        tx = 0;
    }
    if ty + tip_h > win_h {
        ty = win_h - tip_h;
    }
    if ty < 0 {
        ty = 0;
    }

    // Background + border. Use `surface` (not `background`): fix_gdi_alpha
    // intentionally leaves background-coloured pixels at the alpha they had
    // before GDI overwrote them — which for a GDI FillRect is 0 (transparent).
    // A surface colour distinct from the background is restored to full alpha.
    let bg = if theme.surface.to_colorref() == theme.background.to_colorref() {
        theme.bar_background
    } else {
        theme.surface
    };
    fill_rect(mem, tx, ty, tx + tip_w, ty + tip_h, bg);
    let bw = (1.0 * sc).max(1.0) as i32;
    fill_rect(mem, tx, ty, tx + tip_w, ty + bw, theme.accent);
    fill_rect(
        mem,
        tx,
        ty + tip_h - bw,
        tx + tip_w,
        ty + tip_h,
        theme.accent,
    );
    fill_rect(mem, tx, ty, tx + bw, ty + tip_h, theme.accent);
    fill_rect(
        mem,
        tx + tip_w - bw,
        ty,
        tx + tip_w,
        ty + tip_h,
        theme.accent,
    );

    let font = crate::osd::create_font(-(11.0 * sc) as i32, false, "Segoe UI");
    let prev_font = unsafe { SelectObject(mem, font) };

    for (i, line) in lines.iter().enumerate() {
        let lx = tx + pad;
        let ly = ty + pad + i as i32 * line_h;
        tcol(mem, if i == 0 { theme.text } else { theme.text_muted });
        dw(
            mem,
            &mut to_utf16_z(line),
            &mut rct(lx, ly, tx + tip_w - pad, ly + line_h),
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
    }

    unsafe {
        SelectObject(mem, prev_font);
        let _ = DeleteObject(font);
    }
}

// Draw a core group: header line ("P-CORES (8)        Parked 2/8") plus one bar
// per core. Parked cores contribute 0 load (their perf counters are stale while
// parked). Returns the new content_y below the group.
#[allow(clippy::too_many_arguments)]
fn draw_core_group(
    mem: HDC,
    prefix: &str,
    cores: &[usize],
    x: i32,
    y: i32,
    bar_area_w: i32,
    w: i32,
    s: f32,
    scale_mhz: u32,
    cap_mhz: u32,
    collapsed: bool,
    mon: &MonitorState,
    theme: &NativeTheme,
    _fg: Argb,
    dim: Argb,
) -> i32 {
    let hdr_h = (SEC_H as f32 * s) as i32;
    let parked_count = cores
        .iter()
        .filter(|&&i| mon.core_parked.get(i).copied().unwrap_or(false))
        .count();

    let arrow = if collapsed { "\u{25B8}" } else { "\u{25BE}" }; // ▸ / ▾
    tcol(mem, dim);
    dw(
        mem,
        &mut to_utf16_z(&format!("{}  {}-CORES ({})", arrow, prefix, cores.len())),
        &mut rct(x, y, w - x, y + hdr_h),
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );
    dw(
        mem,
        &mut to_utf16_z(&format!("Parked {}/{}", parked_count, cores.len())),
        &mut rct(x, y, w - x, y + hdr_h),
        DT_RIGHT | DT_SINGLELINE | DT_VCENTER,
    );

    if collapsed {
        return y + hdr_h;
    }

    let bar_h = (BAR_H as f32 * s) as i32;
    let bar_gap = (BAR_GAP as f32 * s) as i32;
    let mut cy = y + hdr_h;
    for (i, &ci) in cores.iter().enumerate() {
        let parked = mon.core_parked.get(ci).copied().unwrap_or(false);
        let load = if parked {
            0.0
        } else {
            mon.core_load.get(ci).copied().unwrap_or(0.0)
        };
        let freq_mhz = mon.core_freq_mhz.get(ci).copied().unwrap_or(0);
        draw_core_row(
            mem,
            x,
            cy,
            bar_area_w,
            bar_h,
            s,
            &format!("{}{}", prefix, i),
            load,
            freq_mhz,
            parked,
            scale_mhz,
            cap_mhz,
            theme,
        );
        cy += bar_h + bar_gap;
    }
    cy - bar_gap
}

// One per-core row: label | freq | freq-bar (with cap marker) | load%.
// The bar length encodes frequency on the shared `scale_mhz` scale, so lowering
// Max Processor State or cooling throttling visibly shortens it; the colour
// encodes load. `cap_mhz` draws a vertical marker at the configured ceiling.
fn draw_core_row(
    mem: HDC,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    _sc: f32,
    label: &str,
    load: f32,
    freq_mhz: u32,
    parked: bool,
    scale_mhz: u32,
    cap_mhz: u32,
    theme: &NativeTheme,
) {
    let label_w = (34.0 * _sc) as i32;
    let load_w = (44.0 * _sc) as i32;
    let freq_w = (48.0 * _sc) as i32;
    let gap = (8.0 * _sc) as i32;
    let bar_x = x + label_w + freq_w + gap * 2;
    let bar_w = (w - (bar_x - x) - load_w - gap).max(20);
    let text_col = if parked { theme.text_muted } else { theme.text };
    let load_text = if parked {
        "PARK".to_string()
    } else {
        format!("{:>3}%", (load * 100.0).round() as u32)
    };
    let freq_text = if parked {
        "—".to_string()
    } else if freq_mhz > 0 {
        format!("{:.1}G", freq_mhz as f32 / 1000.0)
    } else {
        "-.-G".to_string()
    };

    tcol(mem, text_col);
    dw(
        mem,
        &mut to_utf16_z(label),
        &mut rct(x, y, x + label_w, y + h),
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );
    dw(
        mem,
        &mut to_utf16_z(&freq_text),
        &mut rct(x + label_w + gap, y, x + label_w + gap + freq_w, y + h),
        DT_RIGHT | DT_SINGLELINE | DT_VCENTER,
    );
    dw(
        mem,
        &mut to_utf16_z(&load_text),
        &mut rct(bar_x + bar_w + gap, y, bar_x + bar_w + gap + load_w, y + h),
        DT_RIGHT | DT_SINGLELINE | DT_VCENTER,
    );

    let bar_h = (CORE_BAR_H as f32 * _sc) as i32;
    let bar_y = y + (h - bar_h) / 2;
    let scale = scale_mhz.max(1) as f32;
    let freq_frac = (freq_mhz as f32 / scale).clamp(0.0, 1.0);
    let cap_frac = if cap_mhz > 0 {
        (cap_mhz as f32 / scale).clamp(0.0, 1.0)
    } else {
        1.0
    };
    draw_core_bar(
        mem, bar_x, bar_y, bar_w, bar_h, freq_frac, load, cap_frac, parked, theme,
    );
}

// Bar fill length = `freq_frac`; colour by `load`; vertical ceiling marker at
// `cap_frac`. Track always drawn so empty space reads as headroom.
fn draw_core_bar(
    mem: HDC,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    freq_frac: f32,
    load: f32,
    cap_frac: f32,
    parked: bool,
    theme: &NativeTheme,
) {
    // Track background for the full bar.
    fill_rect(mem, x, y, x + w, y + h, theme.bar_background);

    if parked {
        // Cap marker still useful, but no fill for parked cores.
        draw_cap_marker(mem, x, y, w, h, cap_frac, theme);
        return;
    }

    let fill_w = (w as f32 * freq_frac) as i32;
    // Fill portion: interpolate between dim and accent based on load
    let fill_color = if load > 0.7 {
        theme.accent
    } else if load > 0.4 {
        // Blend accent and text_muted
        Argb::new(
            255,
            ((theme.accent.r as u32 * 2 + theme.text_muted.r as u32) / 3) as u8,
            ((theme.accent.g as u32 * 2 + theme.text_muted.g as u32) / 3) as u8,
            ((theme.accent.b as u32 * 2 + theme.text_muted.b as u32) / 3) as u8,
        )
    } else {
        // Green-ish tint derived from accent
        Argb::new(
            255,
            (theme.accent.r as u32 * 3 / 4) as u8,
            theme.accent.g,
            (theme.accent.b as u32 * 3 / 4) as u8,
        )
    };

    if fill_w > 0 {
        fill_rect(mem, x, y, x + fill_w, y + h, fill_color);
    }
    draw_cap_marker(mem, x, y, w, h, cap_frac, theme);
}

// Vertical ceiling marker at `cap_frac` of the bar width (Max Processor State).
// Drawn slightly taller than the bar so it stands out against the fill.
fn draw_cap_marker(mem: HDC, x: i32, y: i32, w: i32, h: i32, cap_frac: f32, theme: &NativeTheme) {
    if cap_frac >= 0.999 {
        return;
    } // ceiling at full scale — nothing to show
    let cx = x + (w as f32 * cap_frac) as i32;
    let mw = (h / 8).max(1); // marker width scales with bar height
    let over = (h / 4).max(1);
    fill_rect(mem, cx, y - over, cx + mw, y + h + over, theme.text);
}

fn draw_stress_buttons(
    mem: HDC,
    win_w: i32,
    y: i32,
    h: i32,
    sc: f32,
    mon: &MonitorState,
    theme: &NativeTheme,
) {
    let rects = stress_button_rects(win_w, y, h, sc, mon.core_load.len());
    let pad_sc = (PAD as f32 * sc) as i32;
    tcol(mem, theme.text_muted);
    dw(
        mem,
        &mut to_utf16_z("LOAD"),
        &mut rct(pad_sc, y, pad_sc + (54.0 * sc) as i32, y + h),
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );

    for (level, x1, x2, y1, y2) in rects {
        let label = stress_label(level);
        let is_active = mon.stress_level == level;
        let btn_color = if is_active {
            theme.accent
        } else {
            theme.text_muted
        };
        fill_rect(mem, x1, y1, x2, y2, btn_color);
        tcol(mem, btn_color.contrasting_text_color());
        dw(
            mem,
            &mut to_utf16_z(&label),
            &mut rct(x1, y1, x2, y2),
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
    }
}

pub(super) fn stress_button_rects(
    win_w: i32,
    y: i32,
    h: i32,
    sc: f32,
    core_count: usize,
) -> Vec<(StressLevel, i32, i32, i32, i32)> {
    let levels = stress_levels(core_count);
    let gap = (4.0 * sc) as i32;
    let btn_h = (18.0 * sc) as i32;
    let by = y + (h - btn_h) / 2;
    let widths: Vec<i32> = levels
        .iter()
        .map(|level| match level {
            StressLevel::Off => (36.0 * sc) as i32,
            StressLevel::Threads(n) if *n >= 100 => (38.0 * sc) as i32,
            StressLevel::Threads(n) if *n >= 10 => (30.0 * sc) as i32,
            StressLevel::Threads(_) => (24.0 * sc) as i32,
        })
        .collect();
    let total_w: i32 = widths.iter().sum::<i32>() + gap * (levels.len() as i32 - 1);
    let mut x = win_w - (PAD as f32 * sc) as i32 - total_w;
    let mut rects = Vec::with_capacity(levels.len());
    for (level, width) in levels.into_iter().zip(widths) {
        rects.push((level, x, x + width, by, by + btn_h));
        x += width + gap;
    }
    rects
}

fn stress_levels(core_count: usize) -> Vec<StressLevel> {
    let core_count = core_count.max(1);
    const MAX_NUMERIC_BUTTONS: usize = 6;
    let mut levels = Vec::with_capacity(MAX_NUMERIC_BUTTONS + 1);
    levels.push(StressLevel::Off);

    if core_count == 1 {
        levels.push(StressLevel::Threads(1));
        return levels;
    }

    let step = ((core_count - 1) + (MAX_NUMERIC_BUTTONS - 2)) / (MAX_NUMERIC_BUTTONS - 1);
    let mut value = 1usize;
    while value < core_count && levels.len() < MAX_NUMERIC_BUTTONS {
        levels.push(StressLevel::Threads(value));
        value = (value + step).min(core_count);
    }
    if levels.last().copied() != Some(StressLevel::Threads(core_count)) {
        levels.push(StressLevel::Threads(core_count));
    }
    levels
}

fn stress_label(level: StressLevel) -> String {
    match level {
        StressLevel::Off => "OFF".to_string(),
        StressLevel::Threads(count) => count.to_string(),
    }
}

// ── Drawing helpers ──────────────────────────────────────────────────

fn fill_rect(dc: HDC, x1: i32, y1: i32, x2: i32, y2: i32, color: Argb) {
    if x2 <= x1 || y2 <= y1 {
        return;
    }
    let br = unsafe { CreateSolidBrush(color.to_colorref()) };
    let r = RECT {
        left: x1,
        top: y1,
        right: x2,
        bottom: y2,
    };
    unsafe {
        let _ = FillRect(dc, &r, br);
        _ = DeleteObject(br);
    }
}

fn tcol(dc: HDC, color: Argb) {
    unsafe {
        let _ = SetTextColor(dc, color.to_colorref());
        _ = SetBkMode(dc, TRANSPARENT);
    }
}

fn dw(dc: HDC, text: &mut [u16], rc: &mut RECT, fmt: DRAW_TEXT_FORMAT) {
    unsafe {
        let _ = DrawTextW(dc, text, rc as *mut RECT, fmt);
    }
}

fn rct(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT {
        left: l,
        top: t,
        right: r,
        bottom: b,
    }
}

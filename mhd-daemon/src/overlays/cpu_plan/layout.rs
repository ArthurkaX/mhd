//! Geometry and hit-testing for the CPU power-plan overlay.

use super::*;

// ── Hit tests ────────────────────────────────────────────────────────

pub(super) fn hit_plan_row(y: i32, sc: f32) -> bool {
    let plan_y = (HDR_H as f32 * sc) as i32;
    let plan_bottom = plan_y + (PLAN_H as f32 * sc) as i32;
    y >= plan_y && y < plan_bottom
}

/// Settings Y ranges (unscaled) — compute them from the layout.
pub(super) fn settings_y_ranges(sc: f32) -> SettingsYRanges {
    let s = sc;
    let mut y = HDR_H + PLAN_H + SEP_H;

    // CORES section
    let cores_sec_y = y;
    y += SEC_H;
    let min_cores_y = y;
    y += ROW_H;
    let max_cores_y = y;
    y += ROW_H;
    let _cores_sec_bot = y;

    // FREQUENCY section
    let freq_sec_y = y;
    y += SEC_H;
    let min_freq_y = y;
    y += ROW_H;
    let max_freq_y = y;
    y += ROW_H;
    let _freq_sec_bot = y;

    // POWER FEATURES section
    let pf_sec_y = y;
    y += SEC_H;
    let autonomous_mode_y = y;
    y += ROW_H;
    let turbo_y = y;
    y += ROW_H;
    let cooling_policy_y = y;
    y += ROW_H;
    let increase_policy_y = y;
    y += ROW_H;
    let pf_sec_bot = y;

    // CORE MANAGEMENT section
    let cm_sec_y = y;
    y += SEC_H;
    let hetero_policy_y = y;
    y += ROW_H;
    let parked_perf_y = y;
    y += ROW_H;

    // Save button row — sits directly under the parameters.
    let save_row_y = y;
    y += BTN_ROW_H;

    // Separator after settings
    y += SEP_H;

    // Monitor header
    let mon_hdr_y = y;
    let _ = y + MON_HDR_H;

    SettingsYRanges {
        cores_sec_y: (cores_sec_y as f32 * s) as i32,
        min_cores_y: (min_cores_y as f32 * s) as i32,
        max_cores_y: (max_cores_y as f32 * s) as i32,
        freq_sec_y: (freq_sec_y as f32 * s) as i32,
        min_freq_y: (min_freq_y as f32 * s) as i32,
        max_freq_y: (max_freq_y as f32 * s) as i32,
        pf_sec_y: (pf_sec_y as f32 * s) as i32,
        autonomous_mode_y: (autonomous_mode_y as f32 * s) as i32,
        turbo_y: (turbo_y as f32 * s) as i32,
        cooling_policy_y: (cooling_policy_y as f32 * s) as i32,
        increase_policy_y: (increase_policy_y as f32 * s) as i32,
        pf_sec_bot: (pf_sec_bot as f32 * s) as i32,
        cm_sec_y: (cm_sec_y as f32 * s) as i32,
        hetero_policy_y: (hetero_policy_y as f32 * s) as i32,
        parked_perf_y: (parked_perf_y as f32 * s) as i32,
        save_row_y: (save_row_y as f32 * s) as i32,
        mon_hdr_y: (mon_hdr_y as f32 * s) as i32,
    }
}

pub(super) struct SettingsYRanges {
    pub(super) cores_sec_y: i32,
    pub(super) min_cores_y: i32,
    pub(super) max_cores_y: i32,
    pub(super) freq_sec_y: i32,
    pub(super) min_freq_y: i32,
    pub(super) max_freq_y: i32,
    pub(super) pf_sec_y: i32,
    pub(super) autonomous_mode_y: i32,
    pub(super) turbo_y: i32,
    pub(super) cooling_policy_y: i32,
    pub(super) increase_policy_y: i32,
    #[allow(dead_code)]
    pub(super) pf_sec_bot: i32,
    pub(super) cm_sec_y: i32,
    pub(super) hetero_policy_y: i32,
    pub(super) parked_perf_y: i32,
    pub(super) save_row_y: i32,
    pub(super) mon_hdr_y: i32,
}

pub(super) fn hit_value_cell(x: i32, y: i32, sc: f32) -> Option<Field> {
    let ranges = settings_y_ranges(sc);
    let sx = (PAD as f32 * sc + (LABEL_W as f32 * sc)) as i32;
    let vw = (VAL_W as f32 * sc) as i32;
    let gap = (PAD_INNER as f32 * sc) as i32;
    let row_h = (ROW_H as f32 * sc) as i32;

    // Core parking min cores row
    if y >= ranges.min_cores_y && y < ranges.min_cores_y + row_h {
        if x >= sx && x < sx + vw {
            return Some(Field::AcMinCores);
        }
        if x >= sx + vw + gap && x < sx + vw + gap + vw {
            return Some(Field::DcMinCores);
        }
    }

    // Core parking max cores row
    if y >= ranges.max_cores_y && y < ranges.max_cores_y + row_h {
        if x >= sx && x < sx + vw {
            return Some(Field::AcMaxCores);
        }
        if x >= sx + vw + gap && x < sx + vw + gap + vw {
            return Some(Field::DcMaxCores);
        }
    }

    // Min Freq row
    if y >= ranges.min_freq_y && y < ranges.min_freq_y + row_h {
        if x >= sx && x < sx + vw {
            return Some(Field::AcMinFreq);
        }
        if x >= sx + vw + gap && x < sx + vw + gap + vw {
            return Some(Field::DcMinFreq);
        }
    }

    // Max Freq row
    if y >= ranges.max_freq_y && y < ranges.max_freq_y + row_h {
        if x >= sx && x < sx + vw {
            return Some(Field::AcMaxFreq);
        }
        if x >= sx + vw + gap && x < sx + vw + gap + vw {
            return Some(Field::DcMaxFreq);
        }
    }

    None
}

pub(super) fn hit_toggle(
    x: i32,
    y: i32,
    sc: f32,
    ac: &mut PlanValues,
    dc: &mut PlanValues,
    dirty: &mut bool,
) -> bool {
    let ranges = settings_y_ranges(sc);
    let row_h = (ROW_H as f32 * sc) as i32;
    let sx = (PAD as f32 * sc + (LABEL_W as f32 * sc)) as i32;
    let vw = (VAL_W as f32 * sc) as i32;
    let _gap = (PAD_INNER as f32 * sc) as i32;

    // Autonomous Mode row
    if y >= ranges.autonomous_mode_y && y < ranges.autonomous_mode_y + row_h {
        let toggle_rects = toggle_rects(sx, ranges.autonomous_mode_y, vw, row_h, sc);
        if x >= toggle_rects.0.0 && x < toggle_rects.0.1 {
            ac.autonomous_mode = !ac.autonomous_mode;
            *dirty = true;
            return true;
        }
        if x >= toggle_rects.1.0 && x < toggle_rects.1.1 {
            dc.autonomous_mode = !dc.autonomous_mode;
            *dirty = true;
            return true;
        }
    }

    // Turbo Boost row
    if y >= ranges.turbo_y && y < ranges.turbo_y + row_h {
        let toggle_rects = toggle_rects(sx, ranges.turbo_y, vw, row_h, sc);
        if x >= toggle_rects.0.0 && x < toggle_rects.0.1 {
            ac.turbo = !ac.turbo;
            *dirty = true;
            return true;
        }
        if x >= toggle_rects.1.0 && x < toggle_rects.1.1 {
            dc.turbo = !dc.turbo;
            *dirty = true;
            return true;
        }
    }

    false
}

pub(super) fn hit_dropdown(
    x: i32,
    y: i32,
    sc: f32,
    ac: &mut PlanValues,
    dc: &mut PlanValues,
    dirty: &mut bool,
) -> bool {
    let ranges = settings_y_ranges(sc);
    let row_h = (ROW_H as f32 * sc) as i32;
    let sx = (PAD as f32 * sc + (LABEL_W as f32 * sc)) as i32;
    let vw = (VAL_W as f32 * sc) as i32;
    let gap = (PAD_INNER as f32 * sc) as i32;

    macro_rules! hit_dd {
        ($row_y:expr, $ac_field:expr, $dc_field:expr, $cycle_fn:expr) => {
            if y >= $row_y && y < $row_y + row_h {
                if x >= sx && x < sx + vw {
                    $ac_field = $cycle_fn($ac_field);
                    *dirty = true;
                    return true;
                }
                if x >= sx + vw + gap && x < sx + vw + gap + vw {
                    $dc_field = $cycle_fn($dc_field);
                    *dirty = true;
                    return true;
                }
            }
        };
    }

    hit_dd!(
        ranges.cooling_policy_y,
        ac.cooling_policy,
        dc.cooling_policy,
        cycle_cooling_policy
    );
    hit_dd!(
        ranges.increase_policy_y,
        ac.increase_policy,
        dc.increase_policy,
        cycle_increase_policy
    );
    hit_dd!(
        ranges.hetero_policy_y,
        ac.hetero_policy,
        dc.hetero_policy,
        cycle_hetero_policy
    );
    hit_dd!(
        ranges.parked_perf_y,
        ac.parked_perf,
        dc.parked_perf,
        cycle_parked_perf
    );

    false
}

pub(super) fn cycle_cooling_policy(v: u32) -> u32 {
    if v == 0 { 1 } else { 0 }
}
pub(super) fn cycle_increase_policy(v: u32) -> u32 {
    if v == 0 { 2 } else { 0 }
}
pub(super) fn cycle_hetero_policy(v: u32) -> u32 {
    match v {
        0 => 2,
        2 => 4,
        4 => 5,
        _ => 0,
    }
}
pub(super) fn cycle_parked_perf(v: u32) -> u32 {
    match v {
        0 => 1,
        1 => 2,
        _ => 0,
    }
}

pub(super) fn toggle_rects(
    sx: i32,
    _ry: i32,
    vw: i32,
    _row_h: i32,
    sc: f32,
) -> ((i32, i32), (i32, i32)) {
    let tog_w = (36.0 * sc) as i32;
    let gap = (PAD_INNER as f32 * sc) as i32;
    // AC toggle centered in AC column
    let ac_x = sx + (vw - tog_w) / 2;
    let ac_x2 = ac_x + tog_w;
    // DC toggle centered in DC column
    let dc_x = sx + vw + gap + (vw - tog_w) / 2;
    let dc_x2 = dc_x + tog_w;
    ((ac_x, ac_x2), (dc_x, dc_x2))
}

pub(super) fn hit_stress_button(
    x: i32,
    y: i32,
    sc: f32,
    mon: &mut MonitorState,
    handles: &mut Vec<std::thread::JoinHandle<()>>,
) -> bool {
    let ranges = settings_y_ranges(sc);
    let load_y = ranges.mon_hdr_y + (MON_HDR_H as f32 * sc) as i32;
    let load_h = (LOAD_ROW_H as f32 * sc) as i32;
    let win_w = (W as f32 * sc) as i32;

    for (level, x1, x2, y1, y2) in
        stress_button_rects(win_w, load_y, load_h, sc, mon.core_load.len())
    {
        if x >= x1 && x < x2 && y >= y1 && y < y2 {
            stop_stress_threads(&mon.stress_stop, handles);
            mon.stress_stop = Arc::new(AtomicBool::new(false));
            mon.stress_level = level;
            if let StressLevel::Threads(count) = level {
                let stop = Arc::new(AtomicBool::new(false));
                mon.stress_stop = stop.clone();
                *handles = spawn_stress_threads(count, stop);
            }
            return true;
        }
    }

    false
}

// Which monitor group header (if any) is under the click, accounting for the
// current scroll offset and collapse state. Returns 'p' or 'e'.
pub(super) fn hit_core_group_header(y: i32, sc: f32, st: &PanelState) -> Option<char> {
    let lt = list_top(sc);
    if y < lt {
        return None;
    }
    let hdr_h = (SEC_H as f32 * sc) as i32;
    let bar_h = (BAR_H as f32 * sc) as i32;
    let bar_gap = (BAR_GAP as f32 * sc) as i32;
    let block = |count: usize, collapsed: bool| -> i32 {
        if count == 0 {
            return 0;
        }
        hdr_h
            + if collapsed {
                0
            } else {
                (bar_h + bar_gap) * count as i32 - bar_gap
            }
    };
    let p = st.monitor.p_cores.len();
    let e = st.monitor.e_cores.len();
    let mut gy = lt - st.scroll;
    if p > 0 {
        if y >= gy && y < gy + hdr_h {
            return Some('p');
        }
        gy += block(p, st.p_collapsed);
    }
    if e > 0 && y >= gy && y < gy + hdr_h {
        return Some('e');
    }
    None
}

pub(super) fn hit_apply(x: i32, y: i32, sc: f32, _st: &PanelState) -> bool {
    let ranges = settings_y_ranges(sc);
    let win_w = (W as f32 * sc) as i32;
    let btn_w = (BTN_W as f32 * sc) as i32;
    let btn_h = (BTN_H as f32 * sc) as i32;
    let btn_y = ranges.save_row_y + ((BTN_ROW_H - BTN_H) as f32 * sc / 2.0) as i32;
    let bx = (win_w - btn_w) / 2;

    y >= btn_y && y < btn_y + btn_h && x >= bx && x < bx + btn_w
}

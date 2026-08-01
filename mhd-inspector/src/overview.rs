//! Decision-focused quota overview.

use eframe::egui;
use mhd_telemetry::import::ImportResult;
use mhd_telemetry::live;
use mhd_telemetry::query::{QuotaSample, SlopeProjection, Utilization};

use crate::anthropic_quota::{AnthropicQuota, QuotaPoint, cycle_boundaries, project_to_reset};
use crate::timefmt;

/// Axis options shared by both charts.
#[derive(Clone, Copy)]
pub struct ChartOpts {
    /// Seconds to add to a UTC timestamp to get local wall-clock time.
    pub local_offset: i64,
    /// Span several cycles instead of clamping the view to the current one.
    pub history: bool,
}

/// Render the main question: is the current budget likely to last until reset?
#[allow(clippy::too_many_arguments)]
pub fn show_overview(
    ui: &mut egui::Ui,
    provider: &str,
    quota_5h: &Option<Utilization>,
    quota_7d: &Option<Utilization>,
    slope_5h: &SlopeProjection,
    slope_7d: &SlopeProjection,
    history: &[QuotaSample],
    import: &Option<ImportResult>,
    live: Option<&live::LiveQuota>,
    anthropic: Option<&AnthropicQuota>,
    anthropic_history: &[QuotaPoint],
    cycle_starts: &[i64],
    opts: ChartOpts,
    _ctx: &egui::Context,
) {
    if let Some(imp) = import
        && imp.skipped_rows > 0
    {
        ui.colored_label(
            egui::Color32::YELLOW,
            format!(
                "Import partial: {} malformed rows skipped",
                imp.skipped_rows
            ),
        );
    }

    if provider == "anthropic" {
        // ── Anthropic section ──
        ui.heading(egui::RichText::new("Anthropic").strong().size(18.0));
        show_anthropic_section(ui, anthropic, anthropic_history, opts);
        return;
    }

    // ── OpenAI (Codex) section ──
    ui.heading(egui::RichText::new("OpenAI (Codex)").strong().size(18.0));
    if let Some(lq) = live {
        show_live_badge(ui, lq);
        ui.add_space(8.0);
    }

    // Prefer the weekly window: it is the only real Codex budget on Pro 5x.
    let (label, quota, slope, nominal_window) = if quota_7d.is_some() {
        ("Weekly budget", quota_7d, slope_7d, 7 * 24 * 3600)
    } else {
        ("5-hour budget", quota_5h, slope_5h, 5 * 3600)
    };

    ui.heading("Will I make it to reset?");
    ui.add_space(4.0);

    let Some(quota) = quota else {
        ui.label("Waiting for live quota data…");
        return;
    };

    // The provider reports its own window length; the nominal one is a fallback.
    let window_seconds = quota
        .window_minutes
        .map(|m| m * 60)
        .unwrap_or(nominal_window);

    let projection = slope.projected_at_reset;
    let exhaustion = slope.projected_exhaustion.as_deref();
    let (verdict, detail, color) = match (exhaustion, projection) {
        (Some(when), _) => (
            "Likely to run out",
            format!("At the current pace, the budget is exhausted in {when}."),
            egui::Color32::from_rgb(235, 94, 94),
        ),
        (None, Some(p)) if p >= 100.0 => (
            "Likely to run out",
            format!("Projection reaches {p:.0}% before reset."),
            egui::Color32::from_rgb(235, 94, 94),
        ),
        (None, Some(p)) => (
            "On pace",
            format!("Projected to use {p:.0}% by reset."),
            egui::Color32::from_rgb(82, 190, 120),
        ),
        _ => (
            "Collecting data",
            "A projection will appear after at least two samples in this window.".into(),
            egui::Color32::from_rgb(220, 175, 70),
        ),
    };

    egui::Frame::none()
        .fill(egui::Color32::from_gray(28))
        .stroke(egui::Stroke::new(1.0, color))
        .rounding(8.0)
        .inner_margin(egui::Margin::symmetric(16.0, 12.0))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(verdict)
                    .size(24.0)
                    .strong()
                    .color(color),
            );
            ui.label(detail);
        });

    ui.add_space(10.0);
    ui.horizontal_wrapped(|ui| {
        stat_card(ui, label, &format!("{:.1}% used", quota.used_percent));
        stat_card(
            ui,
            "Remaining",
            &format!("{:.1}%", (100.0 - quota.used_percent).max(0.0)),
        );
        stat_card(
            ui,
            "Reset",
            &mhd_telemetry::query::time_to_reset(quota.resets_at).unwrap_or_else(|| "—".into()),
        );
        stat_card(
            ui,
            "Projected at reset",
            &projection
                .map(|p| format!("{p:.1}%"))
                .unwrap_or_else(|| "—".into()),
        );
        stat_card(
            ui,
            "Current pace",
            &slope
                .full_slope
                .map(|p| format!("{p:.1}% / h"))
                .unwrap_or_else(|| "—".into()),
        );
    });

    ui.add_space(12.0);
    ui.strong(format!("{label} trajectory"));
    if history.is_empty() {
        ui.label("No quota samples yet. The chart fills as the watcher records live updates.");
    } else {
        show_budget_chart(
            ui,
            history,
            quota.resets_at,
            window_seconds,
            projection,
            cycle_starts,
            opts,
        );
    }
}

fn stat_card(ui: &mut egui::Ui, label: &str, value: &str) {
    egui::Frame::none()
        .fill(egui::Color32::from_gray(30))
        .rounding(6.0)
        .inner_margin(egui::Margin::symmetric(10.0, 7.0))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(label)
                    .size(10.0)
                    .color(egui::Color32::GRAY),
            );
            ui.label(egui::RichText::new(value).size(16.0).strong());
        });
}

fn show_budget_chart(
    ui: &mut egui::Ui,
    samples: &[QuotaSample],
    resets_at: Option<i64>,
    window_seconds: i64,
    projection: Option<f64>,
    cycle_starts: &[i64],
    opts: ChartOpts,
) {
    let width = ui.available_width().max(320.0);
    let height = 235.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let left = rect.left() + 38.0;
    let right = rect.right() - 12.0;
    let top = rect.top() + 12.0;
    let bottom = rect.bottom() - 26.0;
    let first = samples.first().map(|s| s.event_at).unwrap_or(0);
    let last = samples.last().map(|s| s.event_at).unwrap_or(first + 1);
    // History mode spans the data so past resets stay visible; the decision
    // view clamps to the current cycle starting one window before reset.
    let (start, end) = if opts.history {
        let end = last.max(resets_at.unwrap_or(last));
        (first, if end <= first { first + 1 } else { end })
    } else {
        let end = resets_at.filter(|reset| *reset > last).unwrap_or(last);
        let start = resets_at
            .map(|reset| reset.saturating_sub(window_seconds))
            .filter(|start| *start < end)
            .unwrap_or(first);
        (start, end)
    };
    let span = (end - start).max(1) as f32;
    let x = |time: i64| left + (time.saturating_sub(start) as f32 / span) * (right - left);
    let y = |pct: f64| bottom - (pct.clamp(0.0, 100.0) as f32 / 100.0) * (bottom - top);

    for pct in [0.0, 25.0, 50.0, 75.0, 100.0] {
        let py = y(pct);
        painter.line_segment(
            [egui::pos2(left, py), egui::pos2(right, py)],
            egui::Stroke::new(1.0, egui::Color32::from_gray(55)),
        );
        painter.text(
            egui::pos2(left - 5.0, py),
            egui::Align2::RIGHT_CENTER,
            format!("{pct:.0}%"),
            egui::FontId::proportional(10.0),
            egui::Color32::GRAY,
        );
    }

    // Time axis on round local boundaries; skip a label where it would crowd
    // the reset marker, but keep its gridline.
    let step = timefmt::axis_step_seconds(end - start);
    let shifted = start + opts.local_offset;
    let mut t = (shifted.div_euclid(step) + 1) * step - opts.local_offset;
    let mut ticks = 0;
    while t <= end && ticks < 64 {
        let tx = x(t);
        painter.line_segment(
            [egui::pos2(tx, top), egui::pos2(tx, bottom)],
            egui::Stroke::new(1.0, egui::Color32::from_gray(42)),
        );
        if !resets_at.is_some_and(|reset| (x(reset) - tx).abs() < 40.0) {
            painter.text(
                egui::pos2(tx, bottom + 8.0),
                egui::Align2::CENTER_TOP,
                timefmt::axis_label(t, opts.local_offset, end - start),
                egui::FontId::proportional(9.0),
                egui::Color32::GRAY,
            );
        }
        t += step;
        ticks += 1;
    }

    // Past resets read weaker than the red future-reset line.
    for &boundary in cycle_starts {
        if boundary >= start && boundary <= end {
            let bx = x(boundary);
            painter.line_segment(
                [egui::pos2(bx, top), egui::pos2(bx, bottom)],
                egui::Stroke::new(1.0, egui::Color32::from_gray(85)),
            );
        }
    }

    if samples.len() >= 2 {
        let stroke = egui::Stroke::new(2.0, egui::Color32::from_rgb(88, 166, 255));
        // Split at cycle boundaries: a straight line across a reset draws a
        // bogus diagonal between two different cycles.
        let mut boundary_idx = 0;
        let mut segment: Vec<egui::Pos2> = Vec::new();
        for sample in samples {
            if boundary_idx < cycle_starts.len() && sample.event_at >= cycle_starts[boundary_idx] {
                while boundary_idx < cycle_starts.len()
                    && sample.event_at >= cycle_starts[boundary_idx]
                {
                    boundary_idx += 1;
                }
                if segment.len() >= 2 {
                    painter.add(egui::Shape::line(segment, stroke));
                }
                segment = Vec::new();
            }
            segment.push(egui::pos2(x(sample.event_at), y(sample.used_percent)));
        }
        if segment.len() >= 2 {
            painter.add(egui::Shape::line(segment, stroke));
        }
    }

    if let (Some(reset), Some(projected), Some(latest)) = (resets_at, projection, samples.last())
        && reset > latest.event_at
    {
        painter.line_segment(
            [
                egui::pos2(x(latest.event_at), y(latest.used_percent)),
                egui::pos2(x(reset), y(projected)),
            ],
            egui::Stroke::new(1.5, egui::Color32::GRAY),
        );
    }

    if let Some(reset) = resets_at {
        let rx = x(reset);
        painter.line_segment(
            [egui::pos2(rx, top), egui::pos2(rx, bottom)],
            egui::Stroke::new(1.0, egui::Color32::from_rgb(235, 94, 94)),
        );
        // The dated stamp is wide, so anchor it right-aligned near the edge.
        let anchor = if rx > right - 70.0 {
            egui::Align2::RIGHT_TOP
        } else {
            egui::Align2::CENTER_TOP
        };
        painter.text(
            egui::pos2(rx, bottom + 8.0),
            anchor,
            format!("reset · {}", timefmt::stamp(reset, opts.local_offset)),
            egui::FontId::proportional(10.0),
            egui::Color32::from_rgb(235, 94, 94),
        );
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if start <= now && now <= end {
        let nx = x(now);
        painter.line_segment(
            [egui::pos2(nx, top), egui::pos2(nx, bottom)],
            egui::Stroke::new(1.0, egui::Color32::from_rgb(120, 200, 255)),
        );
        painter.text(
            egui::pos2(nx, rect.top()),
            egui::Align2::CENTER_TOP,
            "now",
            egui::FontId::proportional(9.0),
            egui::Color32::from_rgb(120, 200, 255),
        );
    }

    if let Some(pos) = response.hover_pos() {
        // Nearest-sample tooltip: ratio-to-index is wrong whenever the data
        // does not fill the plot width, which history mode widens further.
        if let Some((ts, pct)) = samples
            .iter()
            .min_by(|a, b| {
                (x(a.event_at) - pos.x)
                    .abs()
                    .total_cmp(&(x(b.event_at) - pos.x).abs())
            })
            .map(|s| (s.event_at, s.used_percent))
        {
            painter.text(
                egui::pos2(pos.x + 10.0, pos.y - 25.0),
                egui::Align2::LEFT_TOP,
                format!("{} · {:.1}%", timefmt::stamp(ts, opts.local_offset), pct),
                egui::FontId::proportional(12.0),
                egui::Color32::WHITE,
            );
        }
    }
}

fn show_live_badge(ui: &mut egui::Ui, lq: &live::LiveQuota) {
    let plan = live::display_plan_type(lq.plan_type.as_deref());
    ui.colored_label(
        egui::Color32::LIGHT_GREEN,
        egui::RichText::new(format!("Live · {plan}")).size(11.0),
    );
}

// ── Anthropic section ──────────────────────────────────────────────────────

fn show_anthropic_section(
    ui: &mut egui::Ui,
    anthropic: Option<&AnthropicQuota>,
    history: &[QuotaPoint],
    opts: ChartOpts,
) {
    let Some(aq) = anthropic else {
        ui.label("Waiting for Anthropic quota data…");
        return;
    };

    // Subtitle: live badge for Anthropic
    ui.colored_label(
        egui::Color32::LIGHT_GREEN,
        egui::RichText::new("Live").size(11.0),
    );
    ui.add_space(4.0);

    let has_data = aq.h5.is_some() || aq.d7.is_some();
    if !has_data {
        ui.label("No Anthropic quota fields recorded yet.");
        return;
    }

    // Each reset drops utilization back to zero, so the raw history is a
    // sawtooth across windows. Only points inside the CURRENT window may feed
    // the least-squares projection -- regressing across a reset boundary
    // yields a meaningless slope; the cycle view slices the polyline the same
    // way, while history mode deliberately draws the whole sawtooth.
    let current_window = |reset: Option<i64>, window_seconds: i64| -> Vec<QuotaPoint> {
        match reset {
            Some(r) => history
                .iter()
                .copied()
                .filter(|p| p.ts >= r - window_seconds)
                .collect(),
            None => history.to_vec(),
        }
    };
    let h5_history = current_window(aq.h5_reset, 18000);
    let d7_history = current_window(aq.d7_reset, 604800);

    // Pre-compute projections for stat cards and charts
    let h5_projection = aq
        .h5_reset
        .and_then(|reset| project_to_reset(&h5_history, |p| p.h5, reset));
    let d7_projection = aq
        .d7_reset
        .and_then(|reset| project_to_reset(&d7_history, |p| p.d7, reset));

    // Stat cards
    ui.horizontal_wrapped(|ui| {
        if let Some(h5) = aq.h5 {
            stat_card(ui, "5h", &format!("{:.1}% used", h5));
        }
        if let Some(d7) = aq.d7 {
            stat_card(ui, "7d", &format!("{:.1}% used", d7));
        }
        if let Some(reset) = aq.h5_reset {
            stat_card(
                ui,
                "5h reset",
                &mhd_telemetry::query::time_to_reset(Some(reset)).unwrap_or_else(|| "—".into()),
            );
        }
        if let Some(reset) = aq.d7_reset {
            stat_card(
                ui,
                "7d reset",
                &mhd_telemetry::query::time_to_reset(Some(reset)).unwrap_or_else(|| "—".into()),
            );
        }
        if let Some(p) = h5_projection {
            stat_card(ui, "5h projected", &format!("{p:.1}%"));
        }
        if let Some(p) = d7_projection {
            stat_card(ui, "7d projected", &format!("{p:.1}%"));
        }
    });

    ui.add_space(10.0);

    // Charts. History mode feeds the full sawtooth across resets (the
    // projections above stay anchored in the current window); the decision
    // view keeps the sliced current cycle.
    let h5_series: &[QuotaPoint] = if opts.history { history } else { &h5_history };
    let h5_points: Vec<(i64, f64)> = h5_series
        .iter()
        .filter_map(|p| p.h5.map(|v| (p.ts, v)))
        .collect();
    if !h5_points.is_empty() {
        ui.strong("5h trajectory");
        show_window_chart(
            ui,
            &h5_points,
            aq.h5_reset,
            18000,
            h5_projection,
            egui::Color32::from_rgb(88, 166, 255),
            &cycle_boundaries(h5_series, |p| p.h5),
            opts,
        );
        ui.add_space(10.0);
    }

    let d7_series: &[QuotaPoint] = if opts.history { history } else { &d7_history };
    let d7_points: Vec<(i64, f64)> = d7_series
        .iter()
        .filter_map(|p| p.d7.map(|v| (p.ts, v)))
        .collect();
    if !d7_points.is_empty() {
        ui.strong("7d trajectory");
        show_window_chart(
            ui,
            &d7_points,
            aq.d7_reset,
            604800,
            d7_projection,
            egui::Color32::from_rgb(235, 94, 94),
            &cycle_boundaries(d7_series, |p| p.d7),
            opts,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn show_window_chart(
    ui: &mut egui::Ui,
    points: &[(i64, f64)],
    resets_at: Option<i64>,
    window_seconds: i64,
    projection: Option<f64>,
    line_color: egui::Color32,
    cycle_starts: &[i64],
    opts: ChartOpts,
) {
    let width = ui.available_width().max(320.0);
    let height = 180.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let left = rect.left() + 38.0;
    let right = rect.right() - 12.0;
    let top = rect.top() + 12.0;
    let bottom = rect.bottom() - 26.0;
    let first = points.first().map(|p| p.0).unwrap_or(0);
    let last = points.last().map(|p| p.0).unwrap_or(first + 1);
    // History mode spans the data so past resets stay visible; the decision
    // view clamps to the current cycle starting one window before reset.
    let (start, end) = if opts.history {
        let end = last.max(resets_at.unwrap_or(last));
        (first, if end <= first { first + 1 } else { end })
    } else {
        let end = resets_at.filter(|reset| *reset > last).unwrap_or(last);
        let start = resets_at
            .map(|reset| reset.saturating_sub(window_seconds))
            .filter(|start| *start < end)
            .unwrap_or(first);
        (start, end)
    };
    let span = (end - start).max(1) as f32;
    let x = |time: i64| left + (time.saturating_sub(start) as f32 / span) * (right - left);
    let y = |pct: f64| bottom - (pct.clamp(0.0, 100.0) as f32 / 100.0) * (bottom - top);

    for pct in [0.0, 25.0, 50.0, 75.0, 100.0] {
        let py = y(pct);
        painter.line_segment(
            [egui::pos2(left, py), egui::pos2(right, py)],
            egui::Stroke::new(1.0, egui::Color32::from_gray(55)),
        );
        painter.text(
            egui::pos2(left - 5.0, py),
            egui::Align2::RIGHT_CENTER,
            format!("{pct:.0}%"),
            egui::FontId::proportional(10.0),
            egui::Color32::GRAY,
        );
    }

    // Time axis on round local boundaries; skip a label where it would crowd
    // the reset marker, but keep its gridline.
    let step = timefmt::axis_step_seconds(end - start);
    let shifted = start + opts.local_offset;
    let mut t = (shifted.div_euclid(step) + 1) * step - opts.local_offset;
    let mut ticks = 0;
    while t <= end && ticks < 64 {
        let tx = x(t);
        painter.line_segment(
            [egui::pos2(tx, top), egui::pos2(tx, bottom)],
            egui::Stroke::new(1.0, egui::Color32::from_gray(42)),
        );
        if !resets_at.is_some_and(|reset| (x(reset) - tx).abs() < 40.0) {
            painter.text(
                egui::pos2(tx, bottom + 8.0),
                egui::Align2::CENTER_TOP,
                timefmt::axis_label(t, opts.local_offset, end - start),
                egui::FontId::proportional(9.0),
                egui::Color32::GRAY,
            );
        }
        t += step;
        ticks += 1;
    }

    // Past resets read weaker than the red future-reset line.
    for &boundary in cycle_starts {
        if boundary >= start && boundary <= end {
            let bx = x(boundary);
            painter.line_segment(
                [egui::pos2(bx, top), egui::pos2(bx, bottom)],
                egui::Stroke::new(1.0, egui::Color32::from_gray(85)),
            );
        }
    }

    if points.len() >= 2 {
        let stroke = egui::Stroke::new(2.0, line_color);
        // Split at cycle boundaries: a straight line across a reset draws a
        // bogus diagonal between two different cycles.
        let mut boundary_idx = 0;
        let mut segment: Vec<egui::Pos2> = Vec::new();
        for &(ts, pct) in points {
            if boundary_idx < cycle_starts.len() && ts >= cycle_starts[boundary_idx] {
                while boundary_idx < cycle_starts.len() && ts >= cycle_starts[boundary_idx] {
                    boundary_idx += 1;
                }
                if segment.len() >= 2 {
                    painter.add(egui::Shape::line(segment, stroke));
                }
                segment = Vec::new();
            }
            segment.push(egui::pos2(x(ts), y(pct)));
        }
        if segment.len() >= 2 {
            painter.add(egui::Shape::line(segment, stroke));
        }
    }

    if let (Some(reset), Some(projected), Some(latest)) = (resets_at, projection, points.last())
        && reset > latest.0
    {
        painter.line_segment(
            [
                egui::pos2(x(latest.0), y(latest.1)),
                egui::pos2(x(reset), y(projected)),
            ],
            egui::Stroke::new(1.5, egui::Color32::GRAY),
        );
    }

    if let Some(reset) = resets_at {
        let rx = x(reset);
        painter.line_segment(
            [egui::pos2(rx, top), egui::pos2(rx, bottom)],
            egui::Stroke::new(1.0, egui::Color32::from_rgb(235, 94, 94)),
        );
        // The dated stamp is wide, so anchor it right-aligned near the edge.
        let anchor = if rx > right - 70.0 {
            egui::Align2::RIGHT_TOP
        } else {
            egui::Align2::CENTER_TOP
        };
        painter.text(
            egui::pos2(rx, bottom + 8.0),
            anchor,
            format!("reset · {}", timefmt::stamp(reset, opts.local_offset)),
            egui::FontId::proportional(10.0),
            egui::Color32::from_rgb(235, 94, 94),
        );
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if start <= now && now <= end {
        let nx = x(now);
        painter.line_segment(
            [egui::pos2(nx, top), egui::pos2(nx, bottom)],
            egui::Stroke::new(1.0, egui::Color32::from_rgb(120, 200, 255)),
        );
        painter.text(
            egui::pos2(nx, rect.top()),
            egui::Align2::CENTER_TOP,
            "now",
            egui::FontId::proportional(9.0),
            egui::Color32::from_rgb(120, 200, 255),
        );
    }

    if let Some(pos) = response.hover_pos() {
        // Nearest-sample tooltip: ratio-to-index is wrong whenever the data
        // does not fill the plot width, which history mode widens further.
        if let Some((ts, pct)) = points
            .iter()
            .min_by(|a, b| (x(a.0) - pos.x).abs().total_cmp(&(x(b.0) - pos.x).abs()))
            .map(|p| (p.0, p.1))
        {
            painter.text(
                egui::pos2(pos.x + 10.0, pos.y - 25.0),
                egui::Align2::LEFT_TOP,
                format!("{} · {:.1}%", timefmt::stamp(ts, opts.local_offset), pct),
                egui::FontId::proportional(12.0),
                egui::Color32::WHITE,
            );
        }
    }
}

//! Overview tab — quota cards and chart.

use eframe::egui;
use mhd_telemetry::import::ImportResult;
use mhd_telemetry::query::{QuotaSample, SlopeProjection, TokenSummary, Utilization};

use crate::app::relative_time;

/// Render the Overview tab content.
pub fn show_overview(
    ui: &mut egui::Ui,
    quota_5h: &Option<Utilization>,
    _quota_7d: &Option<Utilization>,
    slope_5h: &SlopeProjection,
    _slope_7d: &SlopeProjection,
    tokens: &TokenSummary,
    history: &[QuotaSample],
    import: &Option<ImportResult>,
    ctx: &egui::Context,
) {
    // ── Data quality notice ──
    if let Some(imp) = import {
        if imp.skipped_rows > 0 {
            ui.colored_label(egui::Color32::YELLOW, format!("Import partial: {} malformed rows skipped", imp.skipped_rows));
        }
        if imp.sources_imported == 0 && imp.model_calls_added == 0 {
            ui.label("No new data found.");
        }
    }

    // ── Quota cards ──
    egui::Grid::new("quota_cards").min_col_width(130.0).show(ui, |ui| {
        // 5h window cards
        if let Some(q) = quota_5h {
            card(ui, "Quota (5h)", &format!("{:.1}%", q.used_percent));
            card(ui, "Reset", &mhd_telemetry::query::time_to_reset(q.resets_at).unwrap_or_else(|| "—".into()));
        } else {
            card(ui, "Quota (5h)", "—");
            card(ui, "Reset", "—");
        }

        if let Some(p) = slope_5h.projected_at_reset {
            card(ui, "Projected (5h)", &format!("{:.1}%", p));
        } else {
            card(ui, "Projected (5h)", "—");
        }

        if let Some(e) = &slope_5h.projected_exhaustion {
            card(ui, "Exhaustion", e);
        } else {
            card(ui, "Exhaustion", "—");
        }

        if let Some(s) = slope_5h.full_slope {
            card(ui, "Slope (full)", &format!("{:.1}%/h", s));
        } else {
            card(ui, "Slope (full)", "—");
        }

        if let Some(s) = slope_5h.active_slope {
            card(ui, "Slope (active)", &format!("{:.1}%/h", s));
        } else {
            card(ui, "Slope (active)", "—");
        }

        card(ui, "Input tokens", &format_num(tokens.input_tokens));
        card(ui, "Cached input", &format!("{} ({:.0}%)", format_num(tokens.cached_input_tokens), tokens.cache_hit.unwrap_or(0.0) * 100.0));
        card(ui, "Output tokens", &format_num(tokens.output_tokens));
    });

    ui.separator();

    // ── Quota chart ──
    if history.is_empty() {
        ui.label("No quota data available for charting.");
    } else {
        show_quota_chart(ui, history, ctx);
    }
}

/// Render a colored card with label and value.
fn card(ui: &mut egui::Ui, label: &str, value: &str) {
    egui::Frame::none()
        .fill(egui::Color32::from_gray(30))
        .rounding(4.0)
        .inner_margin(egui::Margin::symmetric(8.0, 4.0))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.label(egui::RichText::new(label).size(10.0).color(egui::Color32::GRAY));
                ui.strong(egui::RichText::new(value).size(14.0));
            });
        });
    ui.end_row();
}

/// Simple quota chart using egui painting.
fn show_quota_chart(ui: &mut egui::Ui, samples: &[QuotaSample], _ctx: &egui::Context) {
    let Some(max_sample) = samples.iter().max_by_key(|s| s.used_percent as i64) else {
        return;
    };
    let min_ts = samples.first().map(|s| s.event_at).unwrap_or(0);
    let max_ts = samples.last().map(|s| s.event_at).unwrap_or(1);
    let range_ts = (max_ts - min_ts).max(1) as f64;
    let range_pct = (max_sample.used_percent).max(1.0);

    // Downsample if needed
    let display: &[QuotaSample] = if samples.len() > 5000 {
        // Simple stride downsampling
        let _stride = samples.len() / 5000;
        // Return all for now — in real use this would be a filtered vec
        samples
    } else {
        samples
    };

    let height = 200.0;
    let width = ui.available_width().max(200.0);

    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, height),
        egui::Sense::hover(),
    );

    let painter = ui.painter_at(rect);
    let canvas_left = rect.min.x;
    let canvas_bottom = rect.max.y;

    // Draw grid lines
    for pct in [0.0, 25.0, 50.0, 75.0, 100.0] {
        let y = canvas_bottom - (pct / range_pct * height as f64) as f32;
        painter.line_segment(
            [egui::pos2(canvas_left, y), egui::pos2(canvas_left + width as f32, y)],
            egui::Stroke::new(1.0, egui::Color32::from_gray(60)),
        );
        painter.text(
            egui::pos2(canvas_left - 5.0, y),
            egui::Align2::RIGHT_CENTER,
            format!("{:.0}%", pct),
            egui::FontId::proportional(10.0),
            egui::Color32::GRAY,
        );
    }

    // Draw quota line
    if display.len() >= 2 {
        let mut points: Vec<egui::Pos2> = Vec::with_capacity(display.len());
        for sample in display {
            let x = canvas_left + ((sample.event_at - min_ts) as f64 / range_ts * width as f64) as f32;
            let y = canvas_bottom - (sample.used_percent / range_pct * height as f64) as f32;
            points.push(egui::pos2(x, y));
        }
        painter.add(egui::Shape::line(
            points,
            egui::Stroke::new(2.0, egui::Color32::from_rgb(80, 200, 255)),
        ));

        // Hover inspection
        if let Some(pos) = response.hover_pos() {
            let rel_x = (pos.x - canvas_left) / width as f32;
            let idx = ((rel_x as f64) * (display.len() - 1) as f64) as usize;
            let idx = idx.min(display.len() - 1);
            if let Some(sample) = display.get(idx) {
                let tooltip_pos = egui::pos2(pos.x + 10.0, pos.y - 30.0);
                let info = format!("{:.1}%\n{}", sample.used_percent, relative_time(sample.event_at));
                painter.text(
                    tooltip_pos,
                    egui::Align2::LEFT_TOP,
                    info,
                    egui::FontId::proportional(12.0),
                    egui::Color32::WHITE,
                );
            }
        }
    }

    // Y-axis label
    painter.text(
        egui::pos2(canvas_left, rect.min.y),
        egui::Align2::LEFT_TOP,
        "Usage %",
        egui::FontId::proportional(10.0),
        egui::Color32::GRAY,
    );
}

fn format_num(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

//! Activity tab — model calls table.

use eframe::egui;
use mhd_telemetry::query::ActivityRow;

use crate::app::relative_time;

/// Render the Activity tab content.
pub fn show_activity(
    ui: &mut egui::Ui,
    rows: &[ActivityRow],
    _projects: &[String],
    _project_filter: &Option<String>,
) {
    if rows.is_empty() {
        ui.label("No model calls found for the selected period.");
        return;
    }

    // ── Table ──
    egui::ScrollArea::vertical()
        .id_salt("activity_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new("activity_grid")
                .striped(true)
                .min_col_width(60.0)
                .show(ui, |ui| {
                    // Header
                    ui.strong("Time");
                    ui.strong("Provider");
                    ui.strong("Project");
                    ui.strong("Session");
                    ui.strong("Model");
                    ui.strong("Input");
                    ui.strong("Cached");
                    ui.strong("Output");
                    ui.strong("Reasoning");
                    ui.strong("Context %");
                    ui.end_row();

                    for row in rows {
                        ui.label(relative_time(row.event_at));
                        ui.label(&row.provider);
                        ui.label(row.project.as_deref().unwrap_or("—"));
                        // Show short session ID
                        let short_sid = if row.session_id.len() > 12 {
                            format!(
                                "..{}",
                                &row.session_id[row.session_id.len().saturating_sub(10)..]
                            )
                        } else {
                            row.session_id.clone()
                        };
                        ui.label(short_sid);
                        ui.label(row.model.as_deref().unwrap_or("—"));
                        ui.label(format_num(row.input_tokens));
                        ui.label(format_num(row.cached_input_tokens));
                        ui.label(format_num(row.output_tokens));
                        ui.label(format_num(row.reasoning_tokens));

                        // Context %
                        let ctx_pct = row
                            .context_window
                            .filter(|cw| *cw > 0)
                            .map(|cw| row.total_tokens as f64 / cw as f64 * 100.0)
                            .map(|p| format!("{:.0}%", p))
                            .unwrap_or_else(|| "—".into());
                        ui.label(ctx_pct);

                        ui.end_row();
                    }
                });
        });
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

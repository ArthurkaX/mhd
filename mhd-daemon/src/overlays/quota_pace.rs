//! Current subscription quota summary for the system-tray tooltip.

use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone)]
struct QuotaCard {
    name: &'static str,
    used: Option<f64>,
    reset: Option<i64>,
}

/// Short system-tray tooltip containing the current 7-day quota summary.
pub fn tray_tooltip() -> String {
    current_cards()
        .iter()
        .map(tray_tooltip_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn tray_tooltip_line(card: &QuotaCard) -> String {
    let Some(used) = card.used else {
        return format!("{}: quota data unavailable", card.name);
    };
    let Some(reset) = card.reset else {
        return format!("{}: spent 7d {:.0}% · reset unknown", card.name, used);
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let seconds_left = reset.saturating_sub(now).max(0);
    // Deviation from a uniform-consumption line stretched across the 7-day
    // window: +N = ahead (overspending), -N = behind (slack left).
    let elapsed_frac = (1.0 - (seconds_left as f64 / (7.0 * 86_400.0))).clamp(0.0, 1.0);
    let pace = used - elapsed_frac * 100.0;
    let until_reset =
        mhd_telemetry::query::time_to_reset(Some(reset)).unwrap_or_else(|| "unknown".into());
    format!(
        "{}: spent 7d {:.0}% · pace {:+.0} · reset {}",
        card.name, used, pace, until_reset
    )
}

fn current_cards() -> Vec<QuotaCard> {
    let codex = mhd_telemetry::db::open_readonly(&mhd_telemetry::db::default_db_path())
        .ok()
        .and_then(|db| mhd_telemetry::query::current_utilization(&db, "codex", "7d"));
    let anthropic_live = crate::llm_proxy::get_quota();
    let anthropic_saved =
        ::llm_proxy::db_log::read_latest_quota(&::llm_proxy::config::config_dir().join("proxy.db"));
    vec![
        QuotaCard {
            name: "Codex",
            used: codex.as_ref().map(|q| q.used_percent),
            reset: codex.and_then(|q| q.resets_at),
        },
        QuotaCard {
            name: "Anthropic",
            used: anthropic_live
                .as_ref()
                .and_then(|q| q.d7_utilization)
                .or_else(|| anthropic_saved.as_ref().and_then(|q| q.d7_utilization))
                .map(|v| v * 100.0),
            reset: anthropic_live
                .as_ref()
                .and_then(|q| q.d7_reset)
                .or_else(|| anthropic_saved.and_then(|q| q.d7_reset)),
        },
    ]
}

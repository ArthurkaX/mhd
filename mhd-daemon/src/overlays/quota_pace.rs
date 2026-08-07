//! Current subscription quota summary for the system-tray tooltip.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Age past which the tooltip admits the number may be out of date. Sits
/// above the 10-minute background cadence so a normally-polling system never
/// shows the marker — it appears only when the poller is off, suspended on a
/// 401, or the machine is offline.
const STALE_AFTER: Duration = Duration::from_secs(15 * 60);

/// How long a computed card set is reused before a hover recomputes it. Hovers
/// fire on every `WM_MOUSEMOVE`; without this each one would open two SQLite
/// connections (telemetry.db + proxy.db) on the GUI thread.
const CARDS_TTL: Duration = Duration::from_secs(2);

/// Cached card set, keyed by the wall-clock instant it was computed.
///
/// Why a `OnceLock<Mutex<...>>` rather than just `Mutex`: the cache is
/// initialised lazily on first hover and lives for the process, so a `static`
/// with a const initializer needs the `OnceLock`.
///
/// CRITICAL: the lock must NEVER be held across the SQLite reads or across any
/// call that could re-enter this module — this codebase has already been
/// bitten by a re-entrant `std::sync::Mutex` deadlock that froze the GUI.
static CARDS: OnceLock<Mutex<Option<(Vec<QuotaCard>, Instant)>>> = OnceLock::new();

#[derive(Clone)]
struct QuotaCard {
    name: &'static str,
    used: Option<f64>,
    reset: Option<i64>,
    /// Age of the reading that produced `used`, if known. None for the
    /// proxy.db fallback (the table keeps no write timestamp we trust here).
    age: Option<Duration>,
}

/// Short system-tray tooltip containing the current 7-day quota summary.
pub fn tray_tooltip() -> String {
    cached_cards()
        .iter()
        .map(tray_tooltip_line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Cache wrapper around [`current_cards`]. Returns the fresh cache on a TTL
/// hit; otherwise computes fresh cards, stores them, and returns them.
fn cached_cards() -> Vec<QuotaCard> {
    let cache = CARDS.get_or_init(|| Mutex::new(None));
    // Lock, check TTL, and clone-and-return when fresh.
    {
        let guard = cache.lock().unwrap();
        if let Some((cards, at)) = guard.as_ref()
            && at.elapsed() < CARDS_TTL
        {
            return cards.clone();
        }
    } // Guard DROPPED here — never held across the SQLite reads below.

    // Why fire-and-forget, and why here rather than in `tray_tooltip`: a fresh
    // reading lands a few hundred ms later and the next `WM_MOUSEMOVE`
    // (hovering always produces more) picks it up — blocking the window
    // procedure on HTTP would freeze the tray. Asking from the cache-miss path
    // rate-limits the request to once per `CARDS_TTL`; on every hover it would
    // wake the poller task once per mouse move for nothing.
    crate::llm_proxy::request_quota_refresh();

    // Why the duplicate compute under a race is acceptable: the SQLite reads
    // take milliseconds, and two hovers in the same instant may both miss the
    // TTL and both recompute. The last one to store simply wins; the window is
    // tiny and correctness is unaffected.
    let cards = current_cards();
    let mut guard = cache.lock().unwrap();
    *guard = Some((cards.clone(), Instant::now()));
    cards
}

fn tray_tooltip_line(card: &QuotaCard) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    tray_tooltip_line_at(card, now)
}

/// Format one card with an injectable wall-clock `now` (epoch seconds) so tests
/// can pin the pace and reset math. The public [`tray_tooltip_line`] passes the
/// real clock.
fn tray_tooltip_line_at(card: &QuotaCard, now: i64) -> String {
    let Some(used) = card.used else {
        return format!("{}: quota data unavailable", card.name);
    };
    let Some(reset) = card.reset else {
        return format!("{}: spent 7d {:.0}% · reset unknown", card.name, used);
    };
    let seconds_left = reset.saturating_sub(now).max(0);
    // Deviation from a uniform-consumption line stretched across the 7-day
    // window: +N = ahead (overspending), -N = behind (slack left).
    let elapsed_frac = (1.0 - (seconds_left as f64 / (7.0 * 86_400.0))).clamp(0.0, 1.0);
    let pace = used - elapsed_frac * 100.0;
    let until_reset =
        mhd_telemetry::query::time_to_reset(Some(reset)).unwrap_or_else(|| "unknown".into());
    let mut line = format!(
        "{}: spent 7d {:.0}% · pace {:+.0} · reset {}",
        card.name, used, pace, until_reset
    );
    // Why the marker only when stale: under a normally-polling system a reading
    // is at most ~10 minutes old, so it never shows. It appears only when the
    // poller is off, suspended on a 401, or the machine is offline.
    if let Some(a) = card.age
        && a >= STALE_AFTER
    {
        let minutes = a.as_secs() / 60;
        if minutes >= 60 {
            line = format!("{line} · {}h ago", minutes / 60);
        } else {
            line = format!("{line} · {minutes}m ago");
        }
    }
    line
}

fn current_cards() -> Vec<QuotaCard> {
    let codex = mhd_telemetry::db::open_readonly(&mhd_telemetry::db::default_db_path())
        .ok()
        .and_then(|db| mhd_telemetry::query::current_utilization(&db, "codex", "7d"));
    // The in-memory reading now carries a real age; the proxy.db fallback does
    // not, so it gets `age: None`.
    let anthropic_fresh = crate::llm_proxy::get_quota_fresh();
    let anthropic_saved =
        ::llm_proxy::db_log::read_latest_quota(&::llm_proxy::config::config_dir().join("proxy.db"));
    let codex_now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    vec![
        QuotaCard {
            name: "Codex",
            used: codex.as_ref().map(|q| q.used_percent),
            reset: codex.as_ref().and_then(|q| q.resets_at),
            // `event_at` is epoch seconds; clamp a skewed future timestamp to
            // a zero age rather than a negative one.
            age: codex
                .as_ref()
                .map(|q| Duration::from_secs(codex_now.saturating_sub(q.event_at).max(0) as u64)),
        },
        QuotaCard {
            name: "Claude",
            used: anthropic_fresh
                .as_ref()
                .and_then(|(q, _)| q.d7_utilization)
                .or_else(|| anthropic_saved.as_ref().and_then(|q| q.d7_utilization))
                .map(|v| v * 100.0),
            reset: anthropic_fresh
                .as_ref()
                .and_then(|(q, _)| q.d7_reset)
                .or_else(|| anthropic_saved.and_then(|q| q.d7_reset)),
            age: anthropic_fresh.as_ref().map(|(_, age)| *age),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card_with_age(age: Option<Duration>) -> QuotaCard {
        QuotaCard {
            name: "Codex",
            used: Some(20.0),
            reset: Some(2000000000),
            age,
        }
    }

    /// A card below the stale threshold renders the exact current format.
    #[test]
    fn line_fresh_below_threshold_is_unchanged() {
        let card = card_with_age(Some(STALE_AFTER - Duration::from_secs(1)));
        let line = tray_tooltip_line_at(&card, 1000000000);
        // No age marker, wording identical to today.
        assert!(!line.contains("ago"));
        assert!(line.starts_with("Codex: spent 7d 20%"));
    }

    /// A card at 20 minutes renders the `20m ago` suffix.
    #[test]
    fn line_20_minutes_renders_minutes_suffix() {
        let card = card_with_age(Some(Duration::from_secs(20 * 60)));
        let line = tray_tooltip_line_at(&card, 1000000000);
        assert!(line.ends_with(" · 20m ago"));
    }

    /// A card at 3 hours renders the `3h ago` suffix.
    #[test]
    fn line_3_hours_renders_hours_suffix() {
        let card = card_with_age(Some(Duration::from_secs(3 * 60 * 60)));
        let line = tray_tooltip_line_at(&card, 1000000000);
        assert!(line.ends_with(" · 3h ago"));
    }

    /// A card with no used value still renders the unavailable line.
    #[test]
    fn line_unavailable_ignores_age() {
        let card = QuotaCard {
            name: "Codex",
            used: None,
            reset: None,
            age: Some(Duration::from_secs(20 * 60)),
        };
        let line = tray_tooltip_line_at(&card, 1000000000);
        assert_eq!(line, "Codex: quota data unavailable");
    }
}

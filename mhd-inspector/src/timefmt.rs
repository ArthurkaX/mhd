//! Local-time formatting for chart axes and reset stamps.
//!
//! No chrono/time dependency (the daemon calls Win32 `GetLocalTime`); the local
//! offset is resolved once per process via SQLite and applied to a whole chart
//! span, so labels near a DST transition can be off by one hour — accepted.

use std::sync::OnceLock;

use rusqlite::Connection;

// ── Local offset (via SQLite) ────────────────────────────────────────────────

static LOCAL_OFFSET: OnceLock<i64> = OnceLock::new();

/// Seconds to add to a UTC epoch second to get local wall-clock time, cached
/// once per process. Resolved through SQLite's `strftime(..., 'localtime')`
/// since there is no timezone crate; 0 (UTC) if the query fails.
///
/// A single offset is applied to a whole chart span, so labels on the far side
/// of a DST transition can be off by one hour — accepted trade-off, not a bug.
pub fn local_offset_seconds() -> i64 {
    *LOCAL_OFFSET.get_or_init(|| {
        let Ok(conn) = Connection::open_in_memory() else {
            return 0;
        };
        conn.query_row(
            "SELECT CAST(strftime('%s','now','localtime') AS INTEGER) - CAST(strftime('%s','now') AS INTEGER)",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
    })
}

// ── Civil-date conversion ────────────────────────────────────────────────────

/// Inverse of `anthropic_quota::days_from_civil`: days since 1970-01-01 to a
/// 1-based (year, month, day) civil date (Howard Hinnant's algorithm).
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// Weekday abbreviation for a day count; 1970-01-01 (a Thursday) is day 0.
/// Negative counts wrap via euclidean remainder.
pub fn weekday_short(days: i64) -> &'static str {
    const NAMES: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    NAMES[days.rem_euclid(7) as usize]
}

/// Split a local epoch second into (days since 1970-01-01, seconds-of-day).
/// Euclidean division so pre-1970 timestamps land on the correct day.
fn split_local(local: i64) -> (i64, i64) {
    (local.div_euclid(86_400), local.rem_euclid(86_400))
}

// ── Axis labels and stamps ───────────────────────────────────────────────────

/// Tick spacing chosen so a chart gets roughly 4-8 ticks across a span.
pub fn axis_step_seconds(span_seconds: i64) -> i64 {
    if span_seconds <= 6 * 3600 {
        3600
    } else if span_seconds <= 12 * 3600 {
        2 * 3600
    } else if span_seconds <= 2 * 86400 {
        6 * 3600
    } else if span_seconds <= 8 * 86400 {
        86400
    } else if span_seconds <= 30 * 86400 {
        2 * 86400
    } else {
        7 * 86400
    }
}

/// Axis label for a UTC epoch second, rendered in local time (`ts + offset`).
/// Granularity follows the span: HH:MM under 6h, weekday + time under 3d,
/// weekday + day.month beyond.
pub fn axis_label(ts: i64, offset: i64, span_seconds: i64) -> String {
    let local = ts + offset;
    let (days, sod) = split_local(local);
    let h = sod / 3600;
    let m = (sod % 3600) / 60;
    if span_seconds <= 6 * 3600 {
        format!("{h:02}:{m:02}")
    } else if span_seconds <= 3 * 86400 {
        format!("{} {h:02}:{m:02}", weekday_short(days))
    } else {
        let (_, mo, d) = civil_from_days(days);
        format!("{} {d:02}.{mo:02}", weekday_short(days))
    }
}

/// Full local stamp for a reset marker: `"Wed 12.08 14:00"`.
pub fn stamp(ts: i64, offset: i64) -> String {
    let local = ts + offset;
    let (days, sod) = split_local(local);
    let (_, mo, d) = civil_from_days(days);
    format!(
        "{} {d:02}.{mo:02} {:02}:{:02}",
        weekday_short(days),
        sod / 3600,
        (sod % 3600) / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Local copy of `anthropic_quota::days_from_civil` (Hinnant) for round-trips.
    fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = if m > 2 { m - 3 } else { m + 9 };
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146097 + doe - 719468
    }

    #[test]
    fn civil_from_days_round_trips() {
        for (y, m, d) in [
            (1970_i64, 1_u32, 1_u32),
            (2000_i64, 2_u32, 29_u32),
            (2024_i64, 2_u32, 29_u32),
            (2026_i64, 8_u32, 1_u32),
        ] {
            let days = days_from_civil(y, m as i64, d as i64);
            assert_eq!(civil_from_days(days), (y, m, d));
        }
    }

    #[test]
    fn weekday_known_dates() {
        assert_eq!(weekday_short(0), "Thu");
        // 2026-08-01 = day 20666; 20666 % 7 = 2 -> Sat.
        let days = days_from_civil(2026, 8, 1);
        assert_eq!(days, 20666);
        assert_eq!(weekday_short(days), "Sat");
    }

    #[test]
    fn axis_step_ladder_boundaries() {
        assert_eq!(axis_step_seconds(6 * 3600), 3600);
        assert_eq!(axis_step_seconds(12 * 3600), 2 * 3600);
        assert_eq!(axis_step_seconds(2 * 86400), 6 * 3600);
        assert_eq!(axis_step_seconds(8 * 86400), 86400);
        assert_eq!(axis_step_seconds(30 * 86400), 2 * 86400);
        assert_eq!(axis_step_seconds(30 * 86400 + 1), 7 * 86400);
    }

    #[test]
    fn axis_label_formats() {
        // 2026-08-05 14:00 UTC is a Wednesday (day 20670, index 6).
        let ts = days_from_civil(2026, 8, 5) * 86400 + 14 * 3600;
        assert_eq!(axis_label(ts, 0, 3600), "14:00");
        assert_eq!(axis_label(ts, 0, 86400), "Wed 14:00");
        assert_eq!(axis_label(ts, 0, 4 * 86400), "Wed 05.08");
    }

    #[test]
    fn stamp_shape() {
        let ts = days_from_civil(2026, 8, 5) * 86400 + 14 * 3600;
        assert_eq!(stamp(ts, 0), "Wed 05.08 14:00");
    }

    #[test]
    fn pre_epoch_no_panic() {
        assert_eq!(axis_label(-1, 0, 3600), "23:59");
        assert_eq!(stamp(-1, 0), "Wed 31.12 23:59");
    }
}

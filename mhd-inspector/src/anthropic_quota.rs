//! Anthropic quota reader — reads the `quota` table from proxy.db.
//!
//! The proxy records Anthropic rate-limit header snapshots sparsely (on material
//! change or every 60s) into proxy.db. This module reads the latest snapshot and
//! a recent history window for sparkline rendering.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, params};

/// Latest Anthropic quota snapshot from proxy.db.
#[derive(Debug, Clone)]
pub struct AnthropicQuota {
    pub h5: Option<f64>,
    pub h5_reset: Option<i64>,
    pub d7: Option<f64>,
    pub d7_reset: Option<i64>,
}

/// A single history point for sparkline rendering (ordered by insertion id).
#[derive(Debug, Clone, Copy)]
pub struct QuotaPoint {
    /// Unix epoch seconds, parsed from the `ts` text column.
    pub ts: i64,
    pub h5: Option<f64>,
    pub d7: Option<f64>,
}

/// Read the latest Anthropic quota row from proxy.db. Returns None if the
/// table is empty or the file cannot be opened.
pub fn read_latest(db_path: &Path) -> Option<AnthropicQuota> {
    let conn = open_readonly(db_path)?;
    conn.query_row(
        "SELECT h5_utilization, h5_reset, d7_utilization, d7_reset
         FROM quota ORDER BY id DESC LIMIT 1",
        [],
        |row| {
            // Anthropic rate-limit headers return utilization as a decimal
            // fraction (0.0–1.0). Multiply by 100 for percent.
            Ok(AnthropicQuota {
                h5: row
                    .get::<_, Option<f64>>(0)
                    .ok()
                    .flatten()
                    .map(|v| v * 100.0),
                h5_reset: row.get(1).ok().flatten(),
                d7: row
                    .get::<_, Option<f64>>(2)
                    .ok()
                    .flatten()
                    .map(|v| v * 100.0),
                d7_reset: row.get(3).ok().flatten(),
            })
        },
    )
    .ok()
}

/// Read quota history with a lower time bound (UTC epoch seconds): the newest
/// `limit` rows at or after `since`, returned oldest-first.  The `ts` column is
/// TEXT, so the filter uses SQLite's own `strftime` parser rather than
/// formatting the bound in Rust.
pub fn read_history_since(db_path: &Path, since: i64, limit: usize) -> Vec<QuotaPoint> {
    let Some(conn) = open_readonly(db_path) else {
        return Vec::new();
    };
    let mut stmt = match conn.prepare(
        "SELECT ts, h5_utilization, d7_utilization
         FROM (SELECT id, ts, h5_utilization, d7_utilization
               FROM quota
               WHERE CAST(strftime('%s', ts) AS INTEGER) >= ?1
               ORDER BY id DESC LIMIT ?2)
         ORDER BY id ASC",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map(params![since, limit as i64], |row| {
        let ts_str: String = row.get(0)?;
        Ok(parse_ts(&ts_str).map(|ts| QuotaPoint {
            ts,
            h5: row
                .get::<_, Option<f64>>(1)
                .ok()
                .flatten()
                .map(|v| v * 100.0),
            d7: row
                .get::<_, Option<f64>>(2)
                .ok()
                .flatten()
                .map(|v| v * 100.0),
        }))
    });
    match rows {
        Ok(mapped) => mapped.filter_map(Result::ok).flatten().collect(),
        Err(_) => Vec::new(),
    }
}

/// Parse the proxy.db `ts` column (`YYYY-MM-DD HH:MM:SS.mmm`, UTC) to epoch seconds.
fn parse_ts(s: &str) -> Option<i64> {
    if s.len() < 19 {
        return None;
    }
    let y = s[0..4].parse::<i64>().ok()?;
    let mo = s[5..7].parse::<i64>().ok()?;
    let d = s[8..10].parse::<i64>().ok()?;
    let h = s[11..13].parse::<i64>().ok()?;
    let mi = s[14..16].parse::<i64>().ok()?;
    let sec = s[17..19].parse::<i64>().ok()?;
    Some(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + sec)
}

/// Convert a civil (gregorian) date to days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Least-squares slope over the points, projected forward to `resets_at`.
/// Returns the projected utilization percent at reset, clamped to >= the last
/// observed value. `None` when fewer than 2 points carry a value.
pub fn project_to_reset(
    points: &[QuotaPoint],
    field: fn(&QuotaPoint) -> Option<f64>,
    resets_at: i64,
) -> Option<f64> {
    let pairs: Vec<(f64, f64)> = points
        .iter()
        .filter_map(|p| field(p).map(|v| (p.ts as f64, v)))
        .collect();
    if pairs.len() < 2 {
        return None;
    }
    let n = pairs.len() as f64;
    let sum_x: f64 = pairs.iter().map(|p| p.0).sum();
    let sum_y: f64 = pairs.iter().map(|p| p.1).sum();
    let sum_xx: f64 = pairs.iter().map(|p| p.0 * p.0).sum();
    let sum_xy: f64 = pairs.iter().map(|p| p.0 * p.1).sum();
    let denom = n * sum_xx - sum_x * sum_x;
    if denom == 0.0 {
        return None;
    }
    let slope = (n * sum_xy - sum_x * sum_y) / denom;
    if !slope.is_finite() {
        return None;
    }
    let intercept = (sum_y - slope * sum_x) / n;
    let last_value = pairs.last().map(|p| p.1).unwrap_or(0.0);
    let projected = intercept + slope * resets_at as f64;
    Some(projected.max(last_value).clamp(0.0, 200.0))
}

/// Detect quota window resets in a history series: the `ts` of every point
/// whose field dropped by more than one percent relative to the previous
/// point that carried a value.  Points with no value are skipped, never read
/// as a drop, and the first point is never a boundary.  Mirrors the
/// mhd-telemetry cycle rule; proxy.db's `quota` table exposes no reset
/// timestamp in this history query, so a value drop is the only signal.
pub fn cycle_boundaries(points: &[QuotaPoint], field: fn(&QuotaPoint) -> Option<f64>) -> Vec<i64> {
    let mut boundaries = Vec::new();
    let mut previous: Option<f64> = None;
    for point in points {
        let Some(value) = field(point) else {
            continue;
        };
        if let Some(before) = previous {
            if before - value > 1.0 {
                boundaries.push(point.ts);
            }
        }
        previous = Some(value);
    }
    boundaries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ts_valid() {
        // 2026-07-29 12:00:00.000 UTC
        // midnight = 1785283200, noon = midnight + 12 * 3600 = 1785326400
        assert_eq!(parse_ts("2026-07-29 12:00:00.000"), Some(1785326400));
    }

    #[test]
    fn test_parse_ts_malformed() {
        assert_eq!(parse_ts(""), None);
        assert_eq!(parse_ts("short"), None);
        assert_eq!(parse_ts("2026-07-29"), None); // missing time portion
    }

    #[test]
    fn test_cycle_boundaries_rising_only() {
        let points = [
            QuotaPoint {
                ts: 1000,
                h5: Some(10.0),
                d7: None,
            },
            QuotaPoint {
                ts: 2000,
                h5: Some(20.0),
                d7: None,
            },
            QuotaPoint {
                ts: 3000,
                h5: Some(30.0),
                d7: None,
            },
        ];
        assert!(cycle_boundaries(&points, |p| p.h5).is_empty());
    }

    #[test]
    fn test_cycle_boundaries_one_drop() {
        let points = [
            QuotaPoint {
                ts: 1000,
                h5: Some(50.0),
                d7: None,
            },
            QuotaPoint {
                ts: 2000,
                h5: Some(10.0),
                d7: None,
            },
            QuotaPoint {
                ts: 3000,
                h5: Some(20.0),
                d7: None,
            },
        ];
        assert_eq!(cycle_boundaries(&points, |p| p.h5), vec![2000]);
    }

    #[test]
    fn test_cycle_boundaries_none_gaps_skipped() {
        // The `None` at 2000 is skipped, not treated as a drop to zero; the
        // reset is measured against the previous valued point (50.0 -> 10.0).
        let points = [
            QuotaPoint {
                ts: 1000,
                h5: Some(50.0),
                d7: None,
            },
            QuotaPoint {
                ts: 2000,
                h5: None,
                d7: None,
            },
            QuotaPoint {
                ts: 3000,
                h5: Some(10.0),
                d7: None,
            },
            QuotaPoint {
                ts: 4000,
                h5: Some(20.0),
                d7: None,
            },
        ];
        assert_eq!(cycle_boundaries(&points, |p| p.h5), vec![3000]);
    }

    #[test]
    fn test_cycle_boundaries_sub_one_wobble_is_not_boundary() {
        let points = [
            QuotaPoint {
                ts: 1000,
                h5: Some(50.0),
                d7: None,
            },
            QuotaPoint {
                ts: 2000,
                h5: Some(49.5),
                d7: None,
            },
            QuotaPoint {
                ts: 3000,
                h5: Some(49.8),
                d7: None,
            },
        ];
        assert!(cycle_boundaries(&points, |p| p.h5).is_empty());
    }
}

fn open_readonly(db_path: &Path) -> Option<Connection> {
    Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}

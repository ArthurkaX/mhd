//! Query functions for the telemetry database.
//!
//! All metric functions operate on normalized database records (spec §9).
//! Every aggregate query returns a data-quality signal: Complete, Partial, or Stale.

use rusqlite::params;

use crate::db::TelemetryDb;

/// Data-quality signal for aggregate results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DataQuality {
    #[default]
    Complete,
    Partial,
    Stale,
}

// ── Current utilization ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Utilization {
    pub used_percent: f64,
    pub window_kind: String,
    pub window_minutes: Option<i64>,
    pub resets_at: Option<i64>,
    pub event_at: i64,
    pub quality: DataQuality,
}

/// Get the latest non-null quota sample for the given provider and window kind.
pub fn current_utilization(
    db: &TelemetryDb,
    provider: &str,
    window_kind: &str,
) -> Option<Utilization> {
    let conn = db.conn();
    let mut stmt = conn
        .prepare(
            "SELECT q.used_percent, q.window_kind, q.window_minutes, q.resets_at, q.event_at
             FROM quota_samples q
             WHERE q.provider = ?1 AND q.window_kind = ?2 AND q.used_percent IS NOT NULL
             ORDER BY q.event_at DESC
             LIMIT 1",
        )
        .ok()?;

    stmt.query_row(params![provider, window_kind], |row| {
        Ok(Utilization {
            used_percent: row.get::<_, f64>(0)?,
            window_kind: row.get(1)?,
            window_minutes: row.get(2)?,
            resets_at: row.get(3)?,
            event_at: row.get(4)?,
            quality: DataQuality::Complete,
        })
    })
    .ok()
}

// ── Time to reset ───────────────────────────────────────────────────────

/// Format seconds as a human duration, or return None if `resets_at` is absent.
pub fn time_to_reset(resets_at: Option<i64>) -> Option<String> {
    let at = resets_at?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    let remaining = at.saturating_sub(now);
    if remaining <= 0 {
        return Some("resetting...".into());
    }
    let hours = remaining / 3600;
    let minutes = (remaining % 3600) / 60;
    Some(if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    })
}

// ── Quota time series ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct QuotaSample {
    pub event_at: i64,
    pub used_percent: f64,
}

/// Retrieve quota samples for a time range, one per window kind.
pub fn quota_samples(
    db: &TelemetryDb,
    provider: &str,
    window_kind: &str,
    since: i64,
) -> Vec<QuotaSample> {
    let conn = db.conn();
    let mut stmt = conn
        .prepare(
            "SELECT event_at, used_percent
             FROM quota_samples
             WHERE provider = ?1 AND window_kind = ?2 AND used_percent IS NOT NULL AND event_at >= ?3
             ORDER BY event_at ASC",
        )
        .unwrap();
    stmt.query_map(params![provider, window_kind, since], |row| {
        Ok(QuotaSample {
            event_at: row.get(0)?,
            used_percent: row.get(1)?,
        })
    })
    .unwrap()
    .filter_map(|r| r.ok())
    .collect()
}

// ── Slopes and projections ──────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct SlopeProjection {
    pub full_slope: Option<f64>,
    pub active_slope: Option<f64>,
    pub projected_at_reset: Option<f64>,
    pub projected_exhaustion: Option<String>,
    pub quality: DataQuality,
}

const ACTIVE_GAP_MINUTES: i64 = 30;

/// Calculate slope and projection for one quota cycle.
pub fn slope_and_projection(
    db: &TelemetryDb,
    provider: &str,
    window_kind: &str,
) -> SlopeProjection {
    let mut result = SlopeProjection::default();

    // Get the samples for the current quota cycle (non-decreasing used_percent)
    let conn = db.conn();
    let mut stmt = match conn.prepare(
        "SELECT q.event_at, q.used_percent, q.resets_at
         FROM quota_samples q
         WHERE q.provider = ?1 AND q.window_kind = ?2 AND q.used_percent IS NOT NULL
         ORDER BY q.event_at ASC",
    ) {
        Ok(s) => s,
        Err(_) => return result,
    };

    let samples: Vec<(i64, f64, Option<i64>)> = stmt
        .query_map(params![provider, window_kind], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    if samples.len() < 2 {
        return result;
    }

    // Find the most recent reset boundary
    let last_reset = samples.iter().rev().find_map(|(_, _, r)| *r);
    let cycle_start = last_reset.unwrap_or(0);

    // Samples within the current cycle
    let cycle_samples: Vec<_> = samples
        .iter()
        .filter(|(t, _, _)| *t >= cycle_start)
        .collect();

    if cycle_samples.len() < 2 {
        return result;
    }

    let first = cycle_samples[0];
    let last = cycle_samples[cycle_samples.len() - 1];

    // Full slope
    let wall_hours = (last.0 - first.0) as f64 / 3600.0;
    if wall_hours > 0.0 {
        result.full_slope = Some((last.1 - first.1) / wall_hours);
    }

    // Active slope: skip gaps > ACTIVE_GAP_MINUTES between adjacent samples
    let active_deltas: Vec<(f64, f64)> = cycle_samples
        .windows(2)
        .filter(|w| {
            let gap = w[1].0 - w[0].0;
            gap <= ACTIVE_GAP_MINUTES * 60
        })
        .map(|w| (w[1].1 - w[0].1, (w[1].0 - w[0].0) as f64 / 3600.0))
        .filter(|(_, hours)| *hours > 0.0)
        .collect();

    if !active_deltas.is_empty() {
        let total_change: f64 = active_deltas.iter().map(|(d, _)| d).sum();
        let total_hours: f64 = active_deltas.iter().map(|(_, h)| h).sum();
        result.active_slope = Some(total_change / total_hours);
    }

    // Projection at reset
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    if let (Some(resets_at), Some(full_slope)) = (last_reset, result.full_slope)
        && full_slope > 0.0
    {
        let hours_until = (resets_at - now) as f64 / 3600.0;
        let projected = last.1 + full_slope * hours_until;
        result.projected_at_reset = Some(projected);

        // Projected exhaustion
        if full_slope > 0.0 {
            let hours_to_100 = (100.0 - last.1) / full_slope;
            if hours_to_100.is_finite() && hours_to_100 > 0.0 {
                let exhaustion_time = now + (hours_to_100 * 3600.0) as i64;
                let hours = exhaustion_time.saturating_sub(now) / 3600;
                let minutes = (exhaustion_time.saturating_sub(now) % 3600) / 60;
                if hours > 0 {
                    result.projected_exhaustion = Some(format!("~{hours}h {minutes}m"));
                } else {
                    result.projected_exhaustion = Some(format!("~{minutes}m"));
                }
            }
        }
    }

    result.quality = if last.0 < now - 3600 {
        DataQuality::Stale
    } else if (cycle_samples.len() as i64) < (samples.len() as i64) {
        DataQuality::Partial
    } else {
        DataQuality::Complete
    };

    result
}

// ── Token summary ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct TokenSummary {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_write_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
    pub total_tokens: i64,
    pub cache_hit: Option<f64>,
    pub fresh_input_tokens: i64,
    pub quality: DataQuality,
}

/// Aggregate token counts since a timestamp.
pub fn token_summary(db: &TelemetryDb, since: i64) -> TokenSummary {
    let conn = db.conn();

    let mut result = TokenSummary::default();

    if let Ok(mut stmt) = conn.prepare(
        "SELECT COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(cached_input_tokens), 0),
                COALESCE(SUM(cache_write_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(reasoning_tokens), 0),
                COALESCE(SUM(total_tokens), 0),
                COUNT(*)
         FROM model_calls WHERE event_at >= ?1",
    ) {
        // The closure fills `result` in place, so the query yields unit;
        // a failed query just leaves the zeroed defaults in place.
        let _: Result<(), _> = stmt.query_row(params![since], |row| {
            result.input_tokens = row.get(0)?;
            result.cached_input_tokens = row.get(1)?;
            result.cache_write_tokens = row.get(2)?;
            result.output_tokens = row.get(3)?;
            result.reasoning_tokens = row.get(4)?;
            result.total_tokens = row.get(5)?;
            Ok(())
        });
    }

    // Cache hit ratio
    if result.input_tokens > 0 {
        result.cache_hit = Some(result.cached_input_tokens as f64 / result.input_tokens as f64);
    }

    // Fresh input
    result.fresh_input_tokens = i64::max(result.input_tokens - result.cached_input_tokens, 0);

    result
}

// ── Activity table ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ActivityRow {
    pub event_at: i64,
    pub provider: String,
    pub project: Option<String>,
    pub session_id: String,
    pub model: Option<String>,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
    pub context_window: Option<i64>,
    pub total_tokens: i64,
}

/// List model calls, newest first, with optional filters.
pub fn model_calls_list(
    db: &TelemetryDb,
    since: i64,
    project_filter: Option<&str>,
    limit: u32,
    offset: u32,
) -> Vec<ActivityRow> {
    let conn = db.conn();

    let (where_clause, filter_params): (String, Vec<Box<dyn rusqlite::types::ToSql>>) =
        if let Some(proj) = project_filter {
            (
                " AND s.project = ?2".into(),
                vec![Box::new(since), Box::new(proj.to_string())],
            )
        } else {
            ("".into(), vec![Box::new(since)])
        };

    let sql = format!(
        "SELECT mc.event_at, mc.provider, s.project, s.external_id, mc.model,
                mc.input_tokens, mc.cached_input_tokens, mc.output_tokens,
                mc.reasoning_tokens, mc.context_window, mc.total_tokens
         FROM model_calls mc
         LEFT JOIN sessions s ON mc.session_id = s.id
         WHERE mc.event_at >= ?1 {where_clause}
         ORDER BY mc.event_at DESC
         LIMIT ?{param_idx} OFFSET ?{offset_idx}",
        param_idx = 2 + if project_filter.is_some() { 1 } else { 0 },
        offset_idx = 3 + if project_filter.is_some() { 1 } else { 0 },
    );

    let stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return vec![],
    };

    // Build params dynamically
    let params: Vec<&dyn rusqlite::types::ToSql> =
        filter_params.iter().map(|b| b.as_ref()).collect();

    // We'll just use a simpler approach with two separate queries
    drop(stmt);
    drop(params);

    // Simpler approach: query without project filter in Rust, filter in memory
    // or use two separate prepared statements. Let's do the straightforward thing.
    if let Some(proj) = project_filter {
        let mut stmt = match conn.prepare(
            "SELECT mc.event_at, mc.provider, s.project, s.external_id, mc.model,
                    mc.input_tokens, mc.cached_input_tokens, mc.output_tokens,
                    mc.reasoning_tokens, mc.context_window, mc.total_tokens
             FROM model_calls mc
             LEFT JOIN sessions s ON mc.session_id = s.id
             WHERE mc.event_at >= ?1 AND s.project = ?2
             ORDER BY mc.event_at DESC
             LIMIT ?3 OFFSET ?4",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };

        stmt.query_map(params![since, proj, limit, offset], |row| {
            Ok(ActivityRow {
                event_at: row.get(0)?,
                provider: row.get(1)?,
                project: row.get(2)?,
                session_id: row.get(3)?,
                model: row.get(4)?,
                input_tokens: row.get(5)?,
                cached_input_tokens: row.get(6)?,
                output_tokens: row.get(7)?,
                reasoning_tokens: row.get(8)?,
                context_window: row.get(9)?,
                total_tokens: row.get(10)?,
            })
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    } else {
        let mut stmt = match conn.prepare(
            "SELECT mc.event_at, mc.provider, s.project, s.external_id, mc.model,
                    mc.input_tokens, mc.cached_input_tokens, mc.output_tokens,
                    mc.reasoning_tokens, mc.context_window, mc.total_tokens
             FROM model_calls mc
             LEFT JOIN sessions s ON mc.session_id = s.id
             WHERE mc.event_at >= ?1
             ORDER BY mc.event_at DESC
             LIMIT ?2 OFFSET ?3",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };

        stmt.query_map(params![since, limit, offset], |row| {
            Ok(ActivityRow {
                event_at: row.get(0)?,
                provider: row.get(1)?,
                project: row.get(2)?,
                session_id: row.get(3)?,
                model: row.get(4)?,
                input_tokens: row.get(5)?,
                cached_input_tokens: row.get(6)?,
                output_tokens: row.get(7)?,
                reasoning_tokens: row.get(8)?,
                context_window: row.get(9)?,
                total_tokens: row.get(10)?,
            })
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    }
}

// ── Project list ────────────────────────────────────────────────────────

/// Get distinct project names from imported sessions.
pub fn list_projects(db: &TelemetryDb) -> Vec<String> {
    let conn = db.conn();
    let mut stmt = match conn
        .prepare("SELECT DISTINCT project FROM sessions WHERE project IS NOT NULL ORDER BY project")
    {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    stmt.query_map([], |row| row.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
}

/// Get the timestamp of the most recent sample.
pub fn latest_sample_time(db: &TelemetryDb) -> Option<i64> {
    let conn = db.conn();
    conn.query_row("SELECT MAX(event_at) FROM model_calls", [], |r| r.get(0))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_or_create;

    fn test_db(name: &str) -> TelemetryDb {
        let dir = std::env::temp_dir().join("mhd_telemetry_test_queries");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        open_or_create(&path).unwrap()
    }

    fn seed_test_data(db: &TelemetryDb) {
        let conn = db.conn();
        conn.execute_batch(
            "INSERT INTO sessions (id, provider, external_id, project) VALUES (1, 'codex', 's1', 'my-project');
             INSERT INTO sessions (id, provider, external_id, project) VALUES (2, 'codex', 's2', NULL);
             INSERT INTO model_calls (id, provider, session_id, event_at, source_offset, input_tokens, cached_input_tokens, output_tokens, total_tokens)
             VALUES
                 (1, 'codex', 1, 1000, 100, 1000, 200, 500, 1500),
                 (2, 'codex', 1, 2000, 200, 2000, 400, 1000, 3000),
                 (3, 'codex', 2, 1500, 300, 500, 100, 300, 800);
             INSERT INTO quota_samples (id, provider, session_id, event_at, window_kind, window_minutes, used_percent, resets_at, source_offset)
             VALUES
                 (1, 'codex', 1, 1000, '5h', 300, 10.0, NULL, 100),
                 (2, 'codex', 1, 2000, '5h', 300, 25.0, NULL, 200);"
        ).unwrap();
    }

    #[test]
    fn test_token_summary() {
        let db = test_db("token_summary.db");
        seed_test_data(&db);
        let summary = token_summary(&db, 0);
        assert_eq!(summary.input_tokens, 3500);
        assert_eq!(summary.cached_input_tokens, 700);
        assert_eq!(summary.output_tokens, 1800,);
        assert!((summary.cache_hit.unwrap() - 0.2).abs() < 0.01);
    }

    #[test]
    fn test_current_utilization() {
        let db = test_db("current_util.db");
        seed_test_data(&db);
        let util = current_utilization(&db, "codex", "5h").unwrap();
        assert!((util.used_percent - 25.0).abs() < 0.01);
        assert_eq!(util.window_kind, "5h");
    }

    #[test]
    fn test_list_projects() {
        let db = test_db("projects.db");
        seed_test_data(&db);
        let projects = list_projects(&db);
        assert_eq!(projects, vec!["my-project"]);
    }

    #[test]
    fn test_slope_and_projection() {
        let db = test_db("slope.db");
        seed_test_data(&db);
        let sp = slope_and_projection(&db, "codex", "5h");
        assert!(sp.full_slope.is_some());
        // 25-10=15 over 1000s = ~54 %/hour
        assert!(sp.full_slope.unwrap() > 0.0);
    }

    #[test]
    fn test_time_to_reset() {
        let far_future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 7200;
        let ttr = time_to_reset(Some(far_future));
        assert!(ttr.is_some());
        assert!(ttr.as_ref().unwrap().contains("h") || ttr.as_ref().unwrap().contains("m"));

        // Past reset
        let past = 1000;
        let ttr = time_to_reset(Some(past));
        assert_eq!(ttr.as_deref(), Some("resetting..."));
    }

    #[test]
    fn test_model_calls_list() {
        let db = test_db("model_calls.db");
        seed_test_data(&db);
        let calls = model_calls_list(&db, 0, None, 10, 0);
        assert_eq!(calls.len(), 3);
        // Should be newest first
        assert!(calls[0].event_at >= calls[1].event_at);

        let calls = model_calls_list(&db, 0, Some("my-project"), 10, 0);
        assert_eq!(calls.len(), 2);
    }
}

//! Incremental import engine — scans Codex rollouts and imports into telemetry.db.
//!
//! Tracks per-file cursors in the `sources` table. Designed for periodic invocations:
//! scan → import new rows → return stats. Never blocks on network I/O.

use std::path::Path;

use rusqlite::params;

use crate::codex::{self, CodexEvent, CodexSource};
use crate::db::TelemetryDb;

/// Aggregate result of one import pass.
pub struct ImportResult {
    pub sources_scanned: u32,
    pub sources_imported: u32,
    pub sessions_added: u32,
    pub model_calls_added: u32,
    pub quota_samples_added: u32,
    pub skipped_rows: u32,
    pub errors: Vec<String>,
}

impl ImportResult {
    pub fn is_idle(&self) -> bool {
        self.sources_imported == 0
            && self.sessions_added == 0
            && self.model_calls_added == 0
            && self.quota_samples_added == 0
            && self.errors.is_empty()
    }
}

/// Run one full import pass: discover sources, scan each, import new rows.
pub fn run_import(db: &mut TelemetryDb, codex_home: &Path) -> ImportResult {
    let mut result = ImportResult {
        sources_scanned: 0,
        sources_imported: 0,
        sessions_added: 0,
        model_calls_added: 0,
        quota_samples_added: 0,
        skipped_rows: 0,
        errors: Vec::new(),
    };

    let sources = codex::discover_sources(codex_home);
    result.sources_scanned = sources.len() as u32;

    for src in &sources {
        if let Err(e) = import_source(db, src, &mut result) {
            result
                .errors
                .push(format!("{}: {e}", src.canonical_path.display()));
        }
    }

    result
}

/// Import one source file. Checks cursor, streams new rows, commits in one transaction.
fn import_source(
    db: &mut TelemetryDb,
    src: &CodexSource,
    result: &mut ImportResult,
) -> Result<(), crate::db::TelemetryError> {
    // ── Resolve cursor ──
    let canonical = src.canonical_path.to_string_lossy().to_string();
    let provider = "codex";

    let (cursor_offset, cursor_size, cursor_has_data) = {
        let conn = db.conn();

        let (offset, size): (i64, i64) = conn
            .prepare("SELECT last_offset, last_size FROM sources WHERE canonical_path = ?1")
            .and_then(|mut stmt| {
                stmt.query_row(params![&canonical], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                })
            })
            .unwrap_or((0, 0));

        // Check if sessions exist for this source (determines cursor validity)
        let has_data: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions
             WHERE source_id = (SELECT id FROM sources WHERE canonical_path = ?1)",
                params![&canonical],
                |r| r.get::<_, i64>(0),
            )
            .map(|c| c > 0)
            .unwrap_or(false);

        (offset, size, has_data)
    };

    // ── Determine start offset ──
    // Three cases:
    //   1. File fully consumed AND data exists → skip entirely
    //   2. File truncated or cursor stale (no data) → restart from 0
    //   3. Normal resume → start from cursor_offset
    if cursor_size as u64 == src.len && cursor_offset > 0 && cursor_has_data {
        return Ok(());
    }

    let start_offset = if cursor_size as u64 > src.len || (!cursor_has_data && cursor_offset > 0) {
        0u64
    } else {
        cursor_offset as u64
    };

    // ── Stream parse ──
    let parse = match codex::parse_rollout(&src.canonical_path, start_offset) {
        Ok(p) => p,
        Err(e) => {
            // Record the error in sources table
            let conn = db.conn();
            conn.execute(
                "INSERT INTO sources (provider, canonical_path, last_offset, last_size, last_modified, status, error) \
                 VALUES (?1, ?2, 0, ?3, ?4, 'error', ?5) \
                 ON CONFLICT(canonical_path) DO UPDATE SET status='error', error=?5",
                params![provider, &canonical, src.len as i64, src.modified, e.to_string()],
            ).ok();
            return Ok(());
        }
    };

    if parse.events.is_empty() && parse.skipped == 0 {
        // File hasn't grown (or all rows are already imported)
        // Still update cursor to reflect the current file size.
        let conn = db.conn();
        conn.execute(
            "INSERT INTO sources (provider, canonical_path, file_identity, last_offset, last_size, last_modified, last_imported) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%s','now')) \
             ON CONFLICT(canonical_path) DO UPDATE SET \
             last_size=excluded.last_size, last_modified=excluded.last_modified, last_imported=excluded.last_imported",
            params![provider, &canonical, src.file_identity, parse.final_offset as i64, src.len as i64, src.modified],
        ).ok();
        return Ok(());
    }

    result.sources_imported += 1;
    result.skipped_rows += parse.skipped as u32;

    // ── Insert in one transaction ──
    {
        let conn = db.conn_mut();
        let tx = conn.transaction()?;

        // Upsert the source cursor
        tx.execute(
            "INSERT INTO sources (provider, canonical_path, file_identity, last_offset, last_size, last_modified, last_imported) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%s','now')) \
             ON CONFLICT(canonical_path) DO UPDATE SET \
             file_identity=excluded.file_identity, \
             last_offset=excluded.last_offset, \
             last_size=excluded.last_size, \
             last_modified=excluded.last_modified, \
             last_imported=excluded.last_imported",
            params![provider, &canonical, src.file_identity, parse.final_offset as i64, src.len as i64, src.modified],
        )?;

        // Get the source_id
        let source_id: i64 = tx.query_row(
            "SELECT id FROM sources WHERE canonical_path = ?1",
            params![&canonical],
            |r| r.get(0),
        )?;

        for event in &parse.events {
            match event {
                CodexEvent::SessionMeta {
                    session_id,
                    started_at,
                    cwd,
                    project,
                    model,
                    cli_version,
                } => {
                    let session_id_str = session_id.as_str();
                    // Upsert session
                    let existing_session: Option<i64> = tx
                        .query_row(
                            "SELECT id FROM sessions WHERE provider = ?1 AND external_id = ?2",
                            params![provider, session_id_str],
                            |r| r.get(0),
                        )
                        .ok();

                    if let Some(sid) = existing_session {
                        // Update metadata
                        tx.execute(
                            "UPDATE sessions SET last_event_at = MAX(COALESCE(last_event_at, 0), COALESCE(?1, 0)), \
                             archived = ?2 WHERE id = ?3",
                            params![started_at, src.is_archived as i64, sid],
                        ).ok();
                    } else {
                        tx.execute(
                            "INSERT INTO sessions (provider, external_id, source_id, started_at, last_event_at, cwd, project, model, cli_version, archived) \
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                            params![provider, session_id_str, source_id, started_at, started_at, cwd, project, model, cli_version, src.is_archived as i64],
                        )?;
                        result.sessions_added += 1;
                    }
                }
                CodexEvent::TokenCount {
                    session_id,
                    event_at,
                    input_tokens,
                    cached_input,
                    cache_write,
                    output_tokens,
                    reasoning_tokens,
                    total_tokens,
                    cumulative_tokens,
                    context_window,
                    rate_limits,
                    source_offset,
                } => {
                    // Resolve session
                    let sid: Option<i64> = tx
                        .query_row(
                            "SELECT id FROM sessions WHERE provider = ?1 AND external_id = ?2",
                            params![provider, session_id],
                            |r| r.get(0),
                        )
                        .ok();

                    let sid = match sid {
                        Some(s) => s,
                        None => {
                            // Session not yet seen — create a stub
                            tx.execute(
                                "INSERT OR IGNORE INTO sessions (provider, external_id, source_id) VALUES (?1, ?2, ?3)",
                                params![provider, session_id, source_id],
                            )?;
                            tx.query_row(
                                "SELECT id FROM sessions WHERE provider = ?1 AND external_id = ?2",
                                params![provider, session_id],
                                |r| r.get(0),
                            )?
                        }
                    };

                    // Deduplicate by (session_id, source_offset)
                    let exists: bool = tx
                        .query_row(
                            "SELECT COUNT(*) FROM model_calls WHERE session_id = ?1 AND source_offset = ?2",
                            params![sid, source_offset],
                            |r| r.get::<_, i64>(0),
                        )
                        .map(|c| c > 0)
                        .unwrap_or(false);

                    if !exists {
                        tx.execute(
                            "INSERT INTO model_calls \
                             (provider, session_id, event_at, source_offset, model, context_window, \
                              input_tokens, cached_input_tokens, cache_write_tokens, \
                              output_tokens, reasoning_tokens, total_tokens, cumulative_tokens) \
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                            params![
                                provider, sid, event_at, source_offset,
                                None as Option<&str>, // model — not in token_count events
                                context_window,
                                input_tokens.unwrap_or(0),
                                cached_input.unwrap_or(0),
                                cache_write.unwrap_or(0),
                                output_tokens.unwrap_or(0),
                                reasoning_tokens.unwrap_or(0),
                                total_tokens.unwrap_or(0),
                                cumulative_tokens,
                            ],
                        )?;
                        result.model_calls_added += 1;
                    }

                    // ── Quota samples ──
                    if let Some(rl) = rate_limits {
                        for w in [&rl.primary, &rl.secondary].into_iter().flatten() {
                            let exists_quota: bool = tx
                                .query_row(
                                    "SELECT COUNT(*) FROM quota_samples WHERE provider = ?1 AND session_id = ?2 AND source_offset = ?3 AND window_kind = ?4",
                                    params![provider, sid, source_offset, &w.window_kind],
                                    |r| r.get::<_, i64>(0),
                                )
                                .map(|c| c > 0)
                                .unwrap_or(false);

                            if !exists_quota {
                                tx.execute(
                                    "INSERT INTO quota_samples \
                                     (provider, session_id, event_at, limit_id, limit_name, plan_type, \
                                      window_kind, window_minutes, used_percent, resets_at, \
                                      has_credits, unlimited_credits, credit_balance, source_offset) \
                                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                                    params![
                                        provider, sid, event_at,
                                        rl.limit_id, rl.limit_name, rl.plan_type,
                                        &w.window_kind, w.window_minutes, w.used_percent, w.resets_at,
                                        rl.credits.as_ref().and_then(|c| c.has_credits),
                                        rl.credits.as_ref().and_then(|c| c.unlimited_credits),
                                        rl.credits.as_ref().and_then(|c| c.credit_balance.clone()),
                                        source_offset,
                                    ],
                                )?;
                                result.quota_samples_added += 1;
                            }
                        }
                    }
                }
                CodexEvent::Other => {}
            }
        }

        // Update session last_event_at from model_calls max
        tx.execute_batch(
            "UPDATE sessions SET last_event_at = (SELECT MAX(event_at) FROM model_calls WHERE model_calls.session_id = sessions.id) \
             WHERE last_event_at IS NULL"
        ).ok();

        tx.commit()?;
    }

    Ok(())
}

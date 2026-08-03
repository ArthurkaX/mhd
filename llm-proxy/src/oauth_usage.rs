//! Background poller for Anthropic OAuth usage endpoint.
//!
//! Reads the OAuth access token from the Claude Code credentials file,
//! then polls `https://api.anthropic.com/api/oauth/usage` every 10 minutes
//! (and early on demand from a tray hover), storing the result in the `quota`
//! table of proxy.db.
//!
//! Only active when the credentials file exists and carries a token (Pro/Max
//! subscription billing). API-key users are untouched — the poller skips
//! silently.
//!
//! The poller runs on a separate tokio task inside the proxy's own async
//! runtime and is best-effort: network errors, parse failures, and HTTP errors
//! are logged and the loop continues.

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;

use crate::db_log::QuotaSnapshot;
use crate::state::AppState;

// ── RFC3339 → epoch seconds ──────────────────────────────────────────────

/// Parse an RFC3339 / ISO-8601 timestamp string to Unix epoch SECONDS.
///
/// Handles the forms Anthropic realistically returns:
/// - `2026-07-30T15:04:05Z`
/// - `2026-07-30T15:04:05.123Z` (fractional seconds — truncated, not rounded)
/// - `2026-07-30T15:04:05+00:00` and non-zero offsets like `-07:00`
///
/// Returns `None` on any input it cannot parse rather than guessing.
pub(crate) fn rfc3339_to_epoch(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    // Minimum length: YYYY-MM-DDTHH:MM:SS (19 chars)
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }

    let y: i64 = s[0..4].parse().ok()?;
    let mo: i64 = s[5..7].parse().ok()?;
    let d: i64 = s[8..10].parse().ok()?;
    let h: i64 = s[11..13].parse().ok()?;
    let mi: i64 = s[14..16].parse().ok()?;
    let sec: i64 = s[17..19].parse().ok()?;

    // Basic range checks.
    if mo < 1 || mo > 12 || d < 1 || d > 31 || h > 23 || mi > 59 || sec > 60 {
        return None;
    }

    let mut pos = 19;

    // Optional fractional seconds — skip (truncate, never round).
    if pos < b.len() && b[pos] == b'.' {
        pos += 1;
        while pos < b.len() && b[pos].is_ascii_digit() {
            pos += 1;
        }
    }

    // Optional timezone: Z or ±HH:MM.
    let mut offset_secs: i64 = 0;
    if pos < b.len() {
        if b[pos] == b'Z' {
            // UTC, offset stays zero.
        } else if (b[pos] == b'+' || b[pos] == b'-') && pos + 6 <= b.len() && b[pos + 3] == b':' {
            let sign: i64 = if b[pos] == b'-' { -1 } else { 1 };
            let tz_part = &s[pos + 1..];
            let tz_h: i64 = tz_part[..2].parse().ok()?;
            let tz_m: i64 = tz_part[3..5].parse().ok()?;
            if tz_h > 23 || tz_m > 59 {
                return None;
            }
            // offset_secs is (UTC - local). For "+05:00", offset = +18000,
            // so UTC epoch = local epoch - 18000.
            // For "-07:00", offset = -25200,
            // so UTC epoch = local epoch - (-25200) = local + 25200.
            offset_secs = sign * (tz_h * 3600 + tz_m * 60);
        } else {
            // Unrecognised trailing content.
            return None;
        }
    }

    let days = days_from_civil(y, mo, d);
    let local_epoch = days * 86400 + h * 3600 + mi * 60 + sec;
    Some(local_epoch - offset_secs)
}

/// Days from civil (Gregorian) date to Unix epoch, using Howard Hinnant's
/// algorithm. Same approach as `mhd-inspector`'s `days_from_civil`.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

// ── Credential reader ────────────────────────────────────────────────────

/// Read the OAuth access token from the Claude Code credentials file.
///
/// Path: `~/.claude/.credentials.json` (Unix) or
/// `%USERPROFILE%\.claude\.credentials.json` (Windows).
///
/// Returns `None` when the file is missing, the JSON is unparseable, or the
/// token is absent or empty. This is a normal "not applicable" state (user on
/// API-key billing), NOT an error — the caller skips the poll quietly.
fn read_oauth_token() -> Option<String> {
    let home = dirs::home_dir()?;
    let path = home.join(".claude").join(".credentials.json");
    let content = std::fs::read_to_string(path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;
    let token = parsed.get("claudeAiOauth")?.get("accessToken")?.as_str()?;
    if token.is_empty() {
        return None;
    }
    // Why: the credentials authenticate against this endpoint even after the
    // local expiry timestamp has passed, so honouring `expiresAt` would
    // suppress working polls.
    Some(token.to_string())
}

// ── Response shape ───────────────────────────────────────────────────────

#[derive(Deserialize, Debug, Default)]
struct OauthUsageResponse {
    #[serde(default)]
    five_hour: Option<WindowData>,
    #[serde(default)]
    seven_day: Option<WindowData>,
    #[serde(default)]
    limits: Vec<LimitEntry>,
}

#[derive(Deserialize, Debug, Default, Clone)]
struct WindowData {
    #[serde(default)]
    utilization: Option<f64>,
    #[serde(rename = "used_percentage", default)]
    used_percentage: Option<f64>,
    #[serde(rename = "resets_at", default)]
    resets_at: Option<ResetsAtValue>,
}

#[derive(Deserialize, Debug, Default, Clone)]
struct LimitEntry {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    percent: Option<f64>,
    #[serde(rename = "resets_at", default)]
    resets_at: Option<ResetsAtValue>,
    #[serde(default)]
    scope: Option<LimitScope>,
}

#[derive(Deserialize, Debug, Default, Clone)]
struct LimitScope {
    #[serde(default)]
    model: Option<ScopeModel>,
}

#[derive(Deserialize, Debug, Default, Clone)]
struct ScopeModel {
    #[serde(rename = "display_name", default)]
    display_name: String,
}

/// `resets_at` may be a JSON string (RFC3339) or a JSON number (epoch seconds
/// or milliseconds). Use an untagged enum to accept both forms.
#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
enum ResetsAtValue {
    Str(String),
    Num(f64),
}

/// Normalise a `ResetsAtValue` to Unix epoch seconds.
fn normalize_resets_at(val: &ResetsAtValue) -> Option<i64> {
    match val {
        ResetsAtValue::Str(s) => rfc3339_to_epoch(s).or_else(|| {
            // The string may also be a numeric string (epoch seconds).
            s.parse::<f64>().ok().and_then(normalize_numeric_ts)
        }),
        ResetsAtValue::Num(n) => normalize_numeric_ts(*n),
    }
}

/// Normalise a numeric timestamp. Above `10_000_000_000` treat as milliseconds
/// and divide by 1000; below treat as epoch seconds.
fn normalize_numeric_ts(n: f64) -> Option<i64> {
    if n > 10_000_000_000.0 {
        Some((n / 1000.0) as i64)
    } else {
        Some(n as i64)
    }
}

/// Extract the utilisation fraction (0..1) from a window-data object.
///
/// The endpoint returns PERCENT (0..100). We divide by 100.0 to match the
/// existing `quota` columns which store a FRACTION (0..1). The inspector
/// multiplies by 100 for display. Getting this wrong is a silent 100x error.
fn window_util(window: &WindowData) -> Option<f64> {
    window
        .utilization
        .or(window.used_percentage)
        .map(|v| (v / 100.0).clamp(0.0, 1.0))
}

fn window_reset(window: &WindowData) -> Option<i64> {
    window.resets_at.as_ref().and_then(normalize_resets_at)
}

/// Build a [`QuotaSnapshot`] from the OAuth usage response.
///
/// Header-only fields (`h5_status`, `d7_status`, `representative_claim`,
/// `fallback_status`, `overage_status`) are left as `None` — they are only
/// populated by the response-header path.
fn build_snapshot(resp: &OauthUsageResponse) -> QuotaSnapshot {
    let h5 = resp.five_hour.as_ref();
    let d7 = resp.seven_day.as_ref();

    // Find Fable in the limits array: first entry where kind ==
    // "weekly_scoped" and scope.model.display_name (trimmed, lowercased) is
    // "fable". Do NOT filter on any `is_active` field — inactive Fable entries
    // still carry a valid percent and reset.
    let fable = resp.limits.iter().find(|l| {
        l.kind.trim().eq_ignore_ascii_case("weekly_scoped")
            && l.scope
                .as_ref()
                .and_then(|s| s.model.as_ref())
                .map(|m| m.display_name.trim().eq_ignore_ascii_case("fable"))
                .unwrap_or(false)
    });

    QuotaSnapshot {
        h5_utilization: h5.and_then(window_util),
        h5_reset: h5.and_then(window_reset),
        d7_utilization: d7.and_then(window_util),
        d7_reset: d7.and_then(window_reset),
        fable_utilization: fable
            .and_then(|l| l.percent)
            .map(|v| (v / 100.0).clamp(0.0, 1.0)),
        fable_reset: fable
            .and_then(|l| l.resets_at.as_ref())
            .and_then(normalize_resets_at),
        source: Some("oauth".to_string()),
        ..Default::default()
    }
}

// ── The fetch ────────────────────────────────────────────────────────────

/// Result of a single OAuth usage endpoint call.
///
/// Why: using a typed enum rather than a plain `String` for the error case so
/// the caller can distinguish auth failures (401/403) from transient errors
/// without string-matching the error message — that would be fragile.
#[derive(Debug)]
enum PollOnceError {
    /// HTTP 401 or 403 — OAuth credentials were rejected.
    Auth,
    /// All other failures: network, DNS, timeout, 5xx, parse error, etc.
    Other(String),
}

/// Returns true when the HTTP status indicates an OAuth credential rejection
/// (401 or 403), so the poller can suspend itself rather than retrying into a
/// guaranteed failure every 5 minutes.
fn is_auth_status(status: u16) -> bool {
    status == 401 || status == 403
}

async fn poll_once(state: &Arc<AppState>, token: &str) -> Result<Option<u64>, PollOnceError> {
    let resp = state
        .http
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", "claude-code/2.1.0")
        .send()
        .await
        .map_err(|e| PollOnceError::Other(format!("request failed: {e}")))?;

    let status = resp.status();

    // HTTP 429 with Retry-After: signal the caller to back off.
    if status == 429 {
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        let _body = resp.text().await.unwrap_or_default();
        return Ok(retry_after);
    }

    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let status_code = status.as_u16();
        // Why: 401/403 means the OAuth credentials were rejected — retrying
        // every 5 minutes is pointless and wasteful. Halt the poller until
        // the user cycles the Quota Watcher toggle (which clears the flag).
        if is_auth_status(status_code) {
            return Err(PollOnceError::Auth);
        }
        return Err(PollOnceError::Other(format!("HTTP {status_code}: {body}")));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| PollOnceError::Other(format!("read body failed: {e}")))?;

    let parsed: OauthUsageResponse = serde_json::from_str(&body)
        .map_err(|e| PollOnceError::Other(format!("parse response failed: {e}")))?;

    let snap = build_snapshot(&parsed);
    state.record_quota_polled(snap);

    Ok(None)
}

// ── Background poller task ───────────────────────────────────────────────

/// Background cadence for the OAuth usage poll. Upstream quantises utilisation
/// to whole percentage points, so ~90% of 5-minute samples reported no change;
/// a 10-minute background cadence loses no resolution, and hover-triggered
/// refreshes cover the moment the user actually looks.
const POLL_INTERVAL_SECS: u64 = 600;

/// Floor between two on-demand (hover-triggered) polls, so a hover storm
/// cannot turn into a request storm.
const MIN_ON_DEMAND_INTERVAL: Duration = Duration::from_secs(60);

/// Whether a wakeup should actually hit the network.
///
/// Scheduled ticks always poll. On-demand wakeups are throttled to
/// `MIN_ON_DEMAND_INTERVAL` since the last successful attempt.
fn should_poll(on_demand: bool, last_poll_at: Option<Instant>, now: Instant) -> bool {
    if !on_demand {
        return true;
    }
    match last_poll_at {
        None => true,
        Some(last) => now.duration_since(last) >= MIN_ON_DEMAND_INTERVAL,
    }
}

/// Background poller — spawned by [`crate::start_embedded_with`] as a tokio
/// task. Fetches the OAuth usage endpoint on a 10-minute cadence and writes
/// quota snapshots to proxy.db. The poller also wakes early on demand (a tray
/// hover calls [`AppState::request_quota_refresh`]) so the value the user sees
/// is current.
///
/// Resilience:
/// - 10s initial sleep so the daemon finishes starting.
/// - Errors are logged on first occurrence only (suppressed repeats to avoid
///   endless warning spam when a persistent condition like a revoked token
///   produces the same error every 10 minutes).
/// - HTTP 429 with a `Retry-After` header sleeps that long before resuming
///   the normal cadence.
/// - Missing or empty credentials skip the poll quietly (API-key billing).
/// - The task never panics; all paths are wrapped in error handling.
pub(crate) async fn poll_loop(state: Arc<AppState>) {
    // Let the daemon finish starting up before the first poll.
    tokio::time::sleep(Duration::from_secs(10)).await;

    let mut interval = tokio::time::interval(Duration::from_secs(POLL_INTERVAL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // When the last actual poll happened, for throttling on-demand wakeups.
    let mut last_poll_at: Option<std::time::Instant> = None;

    // Track the last error message to suppress repeated identical warnings.
    let mut last_err: Option<String> = None;
    // Why: separate from last_err so a single network blip never kills polling
    // until restart. Only 401/403 set this; it is cleared on re-enable so the
    // user can cycle the Quota Watcher toggle to retry with fresh credentials.
    let mut auth_failed: bool = false;
    // Track the gate state so we only log on transitions (not every tick).
    let mut last_enabled: Option<bool> = None;

    loop {
        // Why select over the tick and the Notify: a scheduled tick always
        // polls, while a hover-triggered wakeup is only a *hint* that is
        // throttled below so a hover storm cannot hammer the endpoint.
        let on_demand = tokio::select! {
            _ = interval.tick() => false,
            _ = state.quota_refresh.notified() => true,
        };

        // Read the gate flag — the daemon drives this from the Quota Watcher.
        let enabled = state.is_quota_poll_enabled();

        // Why: log only on transitions so a disabled poller doesn't produce
        // one log line every 10 minutes forever.
        match (last_enabled, enabled) {
            (Some(false), true) => {
                tracing::info!("mhd: quota poller enabled");
                // Why: clear auth_failed on re-enable so cycling the toggle
                // retries with freshly written credentials.
                auth_failed = false;
                last_err = None;
            }
            (Some(true), false) => {
                tracing::info!("mhd: quota poller disabled");
            }
            _ => {}
        }
        last_enabled = Some(enabled);

        if !enabled {
            continue;
        }

        if auth_failed {
            continue;
        }

        // Throttle on-demand wakeups; scheduled ticks always poll.
        if !should_poll(on_demand, last_poll_at, std::time::Instant::now()) {
            continue;
        }

        // Read the token fresh each time so credential changes are picked up.
        // Also clear stale error tracking: the auth state changed.
        let token = match read_oauth_token() {
            Some(t) => t,
            None => {
                last_err = None;
                continue;
            }
        };

        // Why: record the attempt here (before the await) so an in-flight poll
        // anchors the throttle; a hover during the poll is coalesced away.
        last_poll_at = Some(std::time::Instant::now());
        match poll_once(&state, &token).await {
            Ok(Some(retry_after)) => {
                // Server asked us to back off (429 with Retry-After).
                let backoff = retry_after.min(300);
                last_err = None;
                tokio::time::sleep(Duration::from_secs(backoff)).await;
            }
            Ok(None) => {
                last_err = None;
            }
            Err(PollOnceError::Auth) => {
                // Log once; subsequent ticks are silently skipped until the
                // user re-enables the Quota Watcher.
                if !auth_failed {
                    tracing::warn!(
                        "mhd: quota poll suspended — OAuth credentials rejected (401/403). \
                         Resume by toggling Quota Watcher off and on again."
                    );
                    auth_failed = true;
                }
            }
            Err(PollOnceError::Other(detail)) => {
                if last_err.as_deref() != Some(&detail) {
                    tracing::warn!("mhd: quota poll error: {detail}");
                    last_err = Some(detail);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── rfc3339_to_epoch ─────────────────────────────────────────────

    /// 2026-07-30T12:00:00Z = July 30 2026 12:00:00 UTC.
    /// The expected value is derived in the comments below rather than copied
    /// from another date library, since this crate has no date dependency.
    #[test]
    fn test_rfc3339_zulu() {
        // 2026-07-30T12:00:00Z
        // days_from_civil(2026, 7, 30) =
        //   y = 2026 (m > 2), era = 2026/400 = 5, yoe = 2026 - 5*400 = 26
        //   mp = 7 - 3 = 4, doy = (153*4 + 2)/5 + 30 - 1 = (614)/5 + 29 = 122 + 29 = 151
        //   doe = 26*365 + 26/4 - 26/100 + 151 = 9490 + 6 - 0 + 151 = 9647
        //   era * 146097 + doe - 719468 = 5*146097 + 9647 - 719468 = 730485 + 9647 - 719468 = 20664
        // So epoch = 20664 * 86400 + 12 * 3600 = 1785369600 + 43200 = 1785412800
        assert_eq!(rfc3339_to_epoch("2026-07-30T12:00:00Z"), Some(1785412800));
    }

    /// Fractional seconds are truncated (not rounded).
    #[test]
    fn test_rfc3339_fractional() {
        // 2026-07-30T12:00:00.999Z ≈ same epoch as above
        assert_eq!(
            rfc3339_to_epoch("2026-07-30T12:00:00.999Z"),
            Some(1785412800)
        );
        // 2026-07-30T12:00:00.000001Z
        assert_eq!(
            rfc3339_to_epoch("2026-07-30T12:00:00.000001Z"),
            Some(1785412800)
        );
    }

    /// Real payload shape from `/api/oauth/usage`: microsecond precision AND an
    /// explicit `+00:00` offset together. Neither the `Z` nor the offset test
    /// covers this combination, and it is the only form the endpoint actually
    /// emits, so it is pinned here against a hand-derived value.
    #[test]
    fn test_rfc3339_fractional_with_offset() {
        // 20664 * 86400 + 9*3600 + 50*60 = 1785369600 + 35400 = 1785405000
        assert_eq!(
            rfc3339_to_epoch("2026-07-30T09:50:00.300374+00:00"),
            Some(1785405000)
        );
    }

    /// Positive offset: +05:00 means local is 5 hours ahead of UTC.
    #[test]
    fn test_rfc3339_positive_offset() {
        // 2026-07-30T17:00:00+05:00 = 2026-07-30T12:00:00Z
        assert_eq!(
            rfc3339_to_epoch("2026-07-30T17:00:00+05:00"),
            Some(1785412800)
        );
    }

    /// Negative offset: -07:00 means local is 7 hours behind UTC.
    #[test]
    fn test_rfc3339_negative_offset() {
        // 2026-07-30T05:00:00-07:00 = 2026-07-30T12:00:00Z
        assert_eq!(
            rfc3339_to_epoch("2026-07-30T05:00:00-07:00"),
            Some(1785412800)
        );
    }

    /// Leap day: 2024-02-29T00:00:00Z
    #[test]
    fn test_rfc3339_leap_day() {
        // days_from_civil(2024, 2, 29) =
        //   y = 2024 - 1 = 2023 (m <= 2)
        //   era = 2023 / 400 = 5, yoe = 2023 - 5*400 = 23
        //   mp = 2 + 9 = 11, doy = (153*11 + 2)/5 + 29 - 1 = (1685)/5 + 28 = 337 + 28 = 365
        //   doe = 23*365 + 23/4 - 23/100 + 365 = 8395 + 5 - 0 + 365 = 8765
        //   era*146097 + doe - 719468 = 5*146097 + 8765 - 719468 = 730485 + 8765 - 719468 = 19782
        // epoch = 19782 * 86400 = 1709164800
        // Let me verify: 2024-01-01 = days_from_civil(2024, 1, 1)
        //   y=2023, era=5, yoe=23, mp=10 (1+9=10)
        //   doy = (153*10+2)/5 + 1 - 1 = 1532/5 = 306
        //   doe = 23*365 + 5 + 306 = 8395 + 5 + 306 = 8706
        //   era*146097 + 8706 - 719468 = 730485 + 8706 - 719468 = 19723
        // 19723 * 86400 = 1704067200
        // Jan 1 to Feb 29 = 31 (Jan) + 29 (Feb) - 1 = 59 days
        // 19723 + 59 = 19782. 19782 * 86400 = 1709164800. Correct!
        assert_eq!(rfc3339_to_epoch("2024-02-29T00:00:00Z"), Some(1709164800));
    }

    /// Malformed: missing T separator.
    #[test]
    fn test_rfc3339_malformed_missing_t() {
        assert_eq!(rfc3339_to_epoch("2026-07-30 12:00:00Z"), None);
    }

    /// Malformed: completely bogus string.
    #[test]
    fn test_rfc3339_malformed_garbage() {
        assert_eq!(rfc3339_to_epoch("not a timestamp"), None);
    }

    /// Empty string.
    #[test]
    fn test_rfc3339_empty() {
        assert_eq!(rfc3339_to_epoch(""), None);
    }

    /// Invalid date: month 13.
    #[test]
    fn test_rfc3339_invalid_month() {
        assert_eq!(rfc3339_to_epoch("2026-13-01T00:00:00Z"), None);
    }

    // ── normalize_numeric_ts ──────────────────────────────────────────

    #[test]
    fn test_normalize_numeric_seconds() {
        assert_eq!(normalize_numeric_ts(1785412800.0), Some(1785412800));
    }

    #[test]
    fn test_normalize_numeric_milliseconds() {
        assert_eq!(normalize_numeric_ts(1785412800000.0), Some(1785412800));
    }

    #[test]
    fn test_normalize_numeric_boundary_above() {
        // Just above the threshold -> milliseconds.
        let ms = 10_000_000_001.0;
        assert_eq!(normalize_numeric_ts(ms), Some((ms / 1000.0) as i64));
    }

    #[test]
    fn test_normalize_numeric_boundary_below() {
        // Just below the threshold -> seconds.
        assert_eq!(normalize_numeric_ts(9_999_999_999.0), Some(9_999_999_999));
    }

    // ── Error classification ──────────────────────────────────────────

    /// 401 must be classified as auth.
    #[test]
    fn test_is_auth_status_401() {
        assert!(is_auth_status(401));
    }

    /// 403 must be classified as auth.
    #[test]
    fn test_is_auth_status_403() {
        assert!(is_auth_status(403));
    }

    /// 429, 500, and other non-200 statuses must NOT be classified as auth.
    #[test]
    fn test_is_auth_status_non_auth() {
        assert!(!is_auth_status(200));
        assert!(!is_auth_status(429));
        assert!(!is_auth_status(500));
        assert!(!is_auth_status(503));
        assert!(!is_auth_status(400));
    }

    /// PollOnceError::Auth is the auth variant — no string payload.
    #[test]
    fn test_poll_once_error_auth_variant() {
        assert!(matches!(PollOnceError::Auth, PollOnceError::Auth));
    }

    /// PollOnceError::Other carries a detail string.
    #[test]
    fn test_poll_once_error_other_variant() {
        let e = PollOnceError::Other("test".into());
        assert!(matches!(&e, PollOnceError::Other(_)));
    }

    // ── should_poll ─────────────────────────────────────────────────────

    /// A scheduled tick always polls, even immediately after a previous poll.
    #[test]
    fn test_should_poll_scheduled_always_polls() {
        let now = Instant::now();
        let last = now.checked_sub(Duration::from_secs(1)).unwrap();
        assert!(should_poll(false, Some(last), now));
        assert!(should_poll(false, None, now));
    }

    /// An on-demand wakeup with no prior poll always polls.
    #[test]
    fn test_should_poll_on_demand_no_prior_polls() {
        let now = Instant::now();
        assert!(should_poll(true, None, now));
    }

    /// An on-demand wakeup 10s after a poll is throttled.
    #[test]
    fn test_should_poll_on_demand_10s_throttled() {
        let now = Instant::now();
        let last = now.checked_sub(Duration::from_secs(10)).unwrap();
        assert!(!should_poll(true, Some(last), now));
    }

    /// An on-demand wakeup 90s after a poll is allowed.
    #[test]
    fn test_should_poll_on_demand_90s_allowed() {
        let now = Instant::now();
        let last = now.checked_sub(Duration::from_secs(90)).unwrap();
        assert!(should_poll(true, Some(last), now));
    }
}

//! Live quota fetching from the Codex backend API.
//!
//! Makes an authenticated HTTP call to `chatgpt.com/backend-api/wham/usage`
//! using the OAuth token from `~/.codex/auth.json`. Returns real-time quota
//! data for the 5h session window and 7d weekly window, plus reset-credit info.
//!
//! Falls back silently — callers should keep showing DB data when this fails.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::query;

// ── Raw API response types ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct BackendUsageResponse {
    plan_type: Option<String>,
    rate_limit: Option<RateLimitBlock>,
    rate_limit_reset_credits: Option<RawResetCredits>,
}

#[derive(Debug, Deserialize)]
struct RateLimitBlock {
    primary_window: Option<RawWindow>,
    secondary_window: Option<RawWindow>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct RawWindow {
    used_percent: Option<f64>,
    limit_window_seconds: Option<f64>,
    reset_at: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct RawResetCredits {
    available_count: Option<i32>,
    total_earned_count: Option<i32>,
    next_expires_at: Option<serde_json::Value>,
    credits: Option<Vec<RawCredit>>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct RawCredit {
    status: Option<String>,
    expires_at: Option<serde_json::Value>,
    granted_at: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct CodexAuthFile {
    tokens: Option<CodexTokens>,
}

#[derive(Debug, Deserialize)]
struct CodexTokens {
    access_token: Option<String>,
    account_id: Option<String>,
}

// ── Public types ───────────────────────────────────────────────────────────

/// Live quota data fetched from the Codex backend API.
#[derive(Debug, Clone)]
pub struct LiveQuota {
    /// 5h session utilization.
    pub session: Option<Utilization>,
    /// 7d weekly utilization.
    pub weekly: Option<Utilization>,
    /// Plan type (e.g. "plus", "pro", "free").
    pub plan_type: Option<String>,
    /// Rate-limit reset credits info.
    pub reset_credits: Option<ResetCredits>,
}

/// A single utilization value with its reset time.
#[derive(Debug, Clone)]
pub struct Utilization {
    pub used_percent: f64,
    pub resets_at: Option<i64>,
}

impl From<Utilization> for query::Utilization {
    fn from(u: Utilization) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        query::Utilization {
            used_percent: u.used_percent,
            window_kind: String::new(),
            window_minutes: None,
            resets_at: u.resets_at,
            event_at: now,
            quality: query::DataQuality::Complete,
        }
    }
}

/// Rate-limit reset credits from the backend API.
#[derive(Debug, Clone)]
pub struct ResetCredits {
    pub available_count: i32,
    pub total_earned_count: Option<i32>,
    pub next_expires_at: Option<i64>,
}

// ── Fetch implementation ───────────────────────────────────────────────────

/// Fetch live quota from the Codex backend API.
///
/// Reads `{codex_home}/auth.json` for the OAuth token, calls the backend
/// usage endpoint, and returns structured quota data. Returns `Err` if
/// auth is missing, network fails, or the response is malformed.
pub fn fetch_live_quota(codex_home: &Path) -> Result<LiveQuota, String> {
    let auth_path = codex_home.join("auth.json");
    let auth_data = std::fs::read_to_string(&auth_path)
        .map_err(|e| format!("read auth.json: {e}"))?;

    let auth: CodexAuthFile =
        serde_json::from_str(&auth_data).map_err(|e| format!("parse auth.json: {e}"))?;

    let token = auth
        .tokens
        .as_ref()
        .and_then(|t| t.access_token.as_ref())
        .ok_or_else(|| "no access_token in auth.json".to_string())?;

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(5))
        .timeout_read(std::time::Duration::from_secs(10))
        .build();

    let response = agent
        .get("https://chatgpt.com/backend-api/wham/usage")
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", "mhd-telemetry/0.1")
        .set("OpenAI-Beta", "codex-1")
        .set("originator", "Codex Desktop")
        .set(
            "ChatGPT-Account-Id",
            auth.tokens
                .as_ref()
                .and_then(|t| t.account_id.as_deref())
                .unwrap_or(""),
        )
        .call()
        .map_err(|e| format!("HTTP request: {e}"))?;

    let payload: BackendUsageResponse = response
        .into_json()
        .map_err(|e| format!("parse response: {e}"))?;

    let session = payload
        .rate_limit
        .as_ref()
        .and_then(|r| r.primary_window.as_ref())
        .and_then(|w| map_window(w, "5h"));

    let weekly = payload
        .rate_limit
        .as_ref()
        .and_then(|r| r.secondary_window.as_ref())
        .and_then(|w| map_window(w, "7d"));

    let reset_credits = payload.rate_limit_reset_credits.as_ref().map(|raw| {
        let available = raw.available_count.unwrap_or(0).max(0);

        let next_expires = raw
            .credits
            .as_ref()
            .and_then(|credits| {
                credits
                    .iter()
                    .filter(|c| c.status.as_deref() == Some("available"))
                    .filter_map(|c| parse_timestamp(&c.expires_at))
                    .min()
            })
            .or_else(|| parse_timestamp(&raw.next_expires_at));

        ResetCredits {
            available_count: available,
            total_earned_count: raw.total_earned_count,
            next_expires_at: next_expires,
        }
    });

    Ok(LiveQuota {
        session,
        weekly,
        plan_type: payload.plan_type,
        reset_credits,
    })
}

fn map_window(raw: &RawWindow, _window_kind: &str) -> Option<Utilization> {
    let used_percent = raw.used_percent.filter(|p| p.is_finite())?;
    let used_percent = used_percent.clamp(0.0, 100.0);

    let resets_at = raw.reset_at.and_then(|r| {
        if r.is_finite() && r > 0.0 {
            // Codex returns reset_at as Unix seconds
            Some(if r < 10_000_000_000.0 { r as i64 } else { (r / 1000.0) as i64 })
        } else {
            None
        }
    });

    Some(Utilization {
        used_percent,
        resets_at,
    })
}

/// Parse a raw JSON value as a Unix timestamp in seconds.
/// Handles: Unix seconds (number < 10B), Unix ms (number >= 10B), numeric string.
fn parse_timestamp(val: &Option<serde_json::Value>) -> Option<i64> {
    let v = val.as_ref()?;
    match v {
        serde_json::Value::Number(n) => {
            let ts = n.as_f64()?;
            Some(if ts < 10_000_000_000.0 { ts as i64 } else { (ts / 1000.0) as i64 })
        }
        serde_json::Value::String(s) => {
            // Try numeric string first
            if let Ok(n) = s.parse::<f64>() {
                return Some(if n < 10_000_000_000.0 { n as i64 } else { (n / 1000.0) as i64 });
            }
            None
        }
        _ => None,
    }
}

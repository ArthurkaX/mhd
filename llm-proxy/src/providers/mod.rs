pub mod anthropic;
pub mod upstream;

use serde_json::Value;
use std::sync::Arc;

use crate::state::AppState;

/// Decrements the in-flight counter when dropped, so it stays accurate even if
/// a request errors out or the future/stream is cancelled mid-flight. Owns an
/// `Arc<AppState>` so it can be moved into a streaming response body and live
/// for the full duration of the stream.
pub struct InflightGuard(pub Arc<AppState>);

impl InflightGuard {
    pub fn new(state: Arc<AppState>) -> Self {
        state
            .inflight
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(state)
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0
            .inflight
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Produce a compact log line from an Anthropic-format request payload.
/// Content bodies are truncated — only structure is shown.
pub fn summarize_payload(payload: &Value) -> String {
    let mut parts = Vec::new();

    if let Some(model) = payload.get("model").and_then(|v| v.as_str()) {
        parts.push(format!("model={}", model));
    }
    if let Some(mt) = payload.get("max_tokens").and_then(|v| v.as_u64()) {
        parts.push(format!("max_tokens={}", mt));
    }
    if let Some(stream) = payload.get("stream").and_then(|v| v.as_bool()) {
        parts.push(format!("stream={}", stream));
    }
    if let Some(system) = payload.get("system") {
        let text = system.to_string();
        if text.len() > 200 {
            let n = text
                .char_indices()
                .nth(200)
                .map(|(i, _)| i)
                .unwrap_or(text.len());
            parts.push(format!("system={}..({}b)", &text[..n], text.len()));
        } else {
            parts.push(format!("system={}", text));
        }
    }
    if let Some(messages) = payload.get("messages").and_then(|v| v.as_array()) {
        let roles: Vec<String> = messages
            .iter()
            .map(|m| {
                let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("?");
                let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let tool_calls = m.get("tool_calls");
                let tool_use = m.get("tool_use_id").or_else(|| m.get("id"));
                if !content.is_empty() {
                    let c = if content.len() > 200 {
                        let n = content
                            .char_indices()
                            .nth(200)
                            .map(|(i, _)| i)
                            .unwrap_or(content.len());
                        format!("{}..({}b)", &content[..n], content.len())
                    } else {
                        content.to_string()
                    };
                    format!("{role}:{c}")
                } else if tool_calls.is_some() {
                    format!("{role}:tool_calls")
                } else if tool_use.is_some() {
                    format!("{role}:tool_result")
                } else {
                    role.to_string()
                }
            })
            .collect();
        parts.push(format!("messages=[{}]", roles.join(", ")));
    }
    if let Some(tools) = payload.get("tools").and_then(|v| v.as_array()) {
        let tool_names: Vec<String> = tools
            .iter()
            .map(|t| {
                let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let desc = t.get("description").and_then(|v| v.as_str()).unwrap_or("");
                if desc.len() > 100 {
                    let n = desc
                        .char_indices()
                        .nth(100)
                        .map(|(i, _)| i)
                        .unwrap_or(desc.len());
                    format!("{name}({}.{}b)", &desc[..n], desc.len())
                } else {
                    format!("{name}({desc})")
                }
            })
            .collect();
        parts.push(format!("tools=[{}]", tool_names.join(", ")));
    }
    if let Some(tc) = payload.get("tool_choice") {
        parts.push(format!("tool_choice={}", tc));
    }
    if let Some(betas) = payload.get("betas").and_then(|v| v.as_array()) {
        let beta_strs: Vec<&str> = betas.iter().filter_map(|v| v.as_str()).collect();
        if !beta_strs.is_empty() {
            parts.push(format!("betas={:?}", beta_strs));
        }
    }
    if let Some(thinking) = payload.get("thinking") {
        let t = thinking.get("type").and_then(|v| v.as_str()).unwrap_or("?");
        let budget = thinking.get("budget_tokens").and_then(|v| v.as_u64());
        if let Some(b) = budget {
            parts.push(format!("thinking={}(budget:{b})", t));
        } else {
            parts.push(format!("thinking={}", t));
        }
    } else {
        parts.push("thinking=not-present".to_string());
    }

    parts.join(" | ")
}

/// Unix epoch time in milliseconds — for inter-request gap measurements.
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Wall-clock timestamp (`YYYY-MM-DD HH:MM:SS.mmm` UTC) for log line correlation.
pub fn now_ms() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    let ms = d.subsec_millis();
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    // Days since epoch, then compute Y/M/D via a simple leap-year-aware algorithm.
    let days = secs / 86400;
    let y = 1970_i64;
    let mut remaining = days;
    let mut year = y;
    loop {
        let days_in_year = if (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0) {
            366
        } else {
            365
        };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        year += 1;
    }
    let month_days = if (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 0usize;
    for (i, &md) in month_days.iter().enumerate() {
        if remaining < md {
            month = i;
            break;
        }
        remaining -= md;
    }
    let day = remaining + 1;
    format!(
        "{Y:04}-{M:02}-{D:02} {h:02}:{m:02}:{s:02}.{ms:03}",
        Y = year,
        M = month + 1,
        D = day
    )
}

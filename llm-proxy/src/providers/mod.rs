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
            parts.push(format!("system={}..({}b)", &text[..200], text.len()));
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
                        format!("{}..({}b)", &content[..200], content.len())
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
                    format!("{name}({}.{}b)", &desc[..100], desc.len())
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

    parts.join(" | ")
}

/// Wall-clock timestamp (`HH:MM:SS.mmm` UTC) for log line correlation.
pub fn now_ms() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    let ms = d.subsec_millis();
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

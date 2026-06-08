//! JSON transformation between Anthropic Messages API and OpenAI Chat
//! Completions / OpenCode format.
//!
//! Anthropic Messages API → OpenAI / OpenCode:
//!   - `messages` stays mostly the same (roles: `user`/`assistant`)
//!   - `system` → inserted as a system message at index 0
//!   - `max_tokens` → `max_tokens`
//!   - `temperature`, `top_p`, `stop_sequences` → pass through
//!   - `model` → mapped according to a configurable table
//!
//! OpenAI / OpenCode → Anthropic Messages API (response):
//!   - `choices[0].message.content` → `content[0].text`
//!   - `usage` → mapped fields

use serde_json::{Map, Value};

/// Transform an incoming Anthropic-format request into OpenAI/OpenCode format.
pub fn anthropic_to_openai(anthropic: Value) -> Value {
    let mut out = Map::new();

    // ── model mapping ──────────────────────────────────────────────
    let model = anthropic
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("claude-sonnet-4-20250514");

    // Map known Anthropic model names to OpenCode-compatible names.
    let openai_model = match model {
        m if m.contains("opus") => "claude-opus-4-20250514",
        m if m.contains("sonnet") => "claude-sonnet-4-20250514",
        m if m.contains("haiku") => "claude-haiku-3-5-20241022",
        other => other, // pass through as-is
    };
    out.insert("model".to_string(), Value::String(openai_model.to_string()));

    // ── messages ───────────────────────────────────────────────────
    let mut openai_messages: Vec<Value> = Vec::new();

    // If there's a system prompt, insert it as a system message at index 0.
    if let Some(system) = anthropic.get("system") {
        let system_text = match system {
            Value::String(s) => s.clone(),
            Value::Array(arr) => arr
                .iter()
                .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if !system_text.is_empty() {
            let mut sys_msg = Map::new();
            sys_msg.insert("role".to_string(), Value::String("system".to_string()));
            sys_msg.insert("content".to_string(), Value::String(system_text));
            openai_messages.push(Value::Object(sys_msg));
        }
    }

    // Copy over messages (user/assistant), expanding tool blocks.
    if let Some(messages) = anthropic.get("messages").and_then(|v| v.as_array()) {
        for msg in messages {
            convert_message(msg, &mut openai_messages);
        }
    }

    out.insert(
        "messages".to_string(),
        Value::Array(openai_messages),
    );

    // ── tools ──────────────────────────────────────────────────────
    if let Some(tools) = anthropic.get("tools").and_then(|v| v.as_array()) {
        let openai_tools: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                let name = t.get("name")?.as_str()?;
                let mut func = Map::new();
                func.insert("name".to_string(), Value::String(name.to_string()));
                if let Some(desc) = t.get("description") {
                    func.insert("description".to_string(), desc.clone());
                }
                // Anthropic `input_schema` → OpenAI `parameters`.
                if let Some(schema) = t.get("input_schema") {
                    func.insert("parameters".to_string(), schema.clone());
                }
                Some(serde_json::json!({ "type": "function", "function": func }))
            })
            .collect();
        if !openai_tools.is_empty() {
            out.insert("tools".to_string(), Value::Array(openai_tools));
        }
    }

    // ── tool_choice ────────────────────────────────────────────────
    if let Some(tc) = anthropic.get("tool_choice") {
        let mapped = match tc.get("type").and_then(|v| v.as_str()) {
            Some("auto") => Value::String("auto".to_string()),
            Some("any") => Value::String("required".to_string()),
            Some("tool") => {
                let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("");
                serde_json::json!({ "type": "function", "function": { "name": name } })
            }
            _ => Value::String("auto".to_string()),
        };
        out.insert("tool_choice".to_string(), mapped);
    }

    // ── parameters ─────────────────────────────────────────────────
    copy_if_present(&mut out, &anthropic, "max_tokens");
    copy_if_present(&mut out, &anthropic, "temperature");
    copy_if_present(&mut out, &anthropic, "top_p");
    copy_if_present(&mut out, &anthropic, "stop_sequences");
    // Anthropic's `stream` flag
    if let Some(stream) = anthropic.get("stream").and_then(|v| v.as_bool()) {
        out.insert("stream".to_string(), Value::Bool(stream));
    }

    Value::Object(out)
}

/// Convert a single Anthropic message into one or more OpenAI messages.
/// Tool results (Anthropic puts them in a user message) become separate OpenAI
/// `tool` messages; tool uses (in an assistant message) become `tool_calls`.
fn convert_message(msg: &Value, out: &mut Vec<Value>) {
    let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");

    // Simple string content → straight passthrough.
    let blocks = match msg.get("content") {
        Some(Value::String(s)) => {
            out.push(serde_json::json!({ "role": role, "content": s }));
            return;
        }
        Some(Value::Array(arr)) => arr,
        _ => {
            out.push(serde_json::json!({ "role": role, "content": "" }));
            return;
        }
    };

    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for block in blocks {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    text_parts.push(t.to_string());
                }
            }
            Some("tool_use") => {
                // Assistant requested a tool call.
                let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = block.get("input").cloned().unwrap_or(serde_json::json!({}));
                let args_str = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
                tool_calls.push(serde_json::json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": args_str }
                }));
            }
            Some("tool_result") => {
                // Result of a previous tool call → its own OpenAI `tool` message.
                let id = block
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let content = match block.get("content") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Array(arr)) => arr
                        .iter()
                        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                out.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": content
                }));
            }
            _ => {}
        }
    }

    // Emit the text/tool_calls message for this role (if anything remains).
    let text = text_parts.join("\n");
    if !tool_calls.is_empty() {
        let mut m = Map::new();
        m.insert("role".to_string(), Value::String(role.to_string()));
        // OpenAI allows null content alongside tool_calls.
        m.insert(
            "content".to_string(),
            if text.is_empty() { Value::Null } else { Value::String(text) },
        );
        m.insert("tool_calls".to_string(), Value::Array(tool_calls));
        out.push(Value::Object(m));
    } else if !text.is_empty() {
        out.push(serde_json::json!({ "role": role, "content": text }));
    }
}

/// Transform an OpenAI/OpenCode chat completion response into Anthropic
/// Messages API response format so Claude Code understands it.
pub fn openai_to_anthropic(openai: Value) -> Value {
    let mut out = Map::new();

    // ── id ─────────────────────────────────────────────────────────
    let id = openai
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("msg_0000000000");
    out.insert("id".to_string(), Value::String(id.to_string()));

    // ── type ───────────────────────────────────────────────────────
    out.insert(
        "type".to_string(),
        Value::String("message".to_string()),
    );

    // ── role ───────────────────────────────────────────────────────
    out.insert(
        "role".to_string(),
        Value::String("assistant".to_string()),
    );

    // ── content (text + tool_use) ──────────────────────────────────
    let message = openai
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|choices| choices.first())
        .and_then(|c| c.get("message"));

    let mut content_blocks: Vec<Value> = Vec::new();

    if let Some(text) = message
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
    {
        if !text.is_empty() {
            content_blocks.push(serde_json::json!({ "type": "text", "text": text }));
        }
    }

    let mut has_tool_use = false;
    if let Some(tool_calls) = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(|t| t.as_array())
    {
        for tc in tool_calls {
            let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let func = tc.get("function");
            let name = func
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let args_str = func
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
                .unwrap_or("{}");
            let input: Value = serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
            content_blocks.push(serde_json::json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input
            }));
            has_tool_use = true;
        }
    }

    if content_blocks.is_empty() {
        content_blocks.push(serde_json::json!({ "type": "text", "text": "" }));
    }
    out.insert("content".to_string(), Value::Array(content_blocks));

    // ── stop_reason ────────────────────────────────────────────────
    let finish_reason = openai
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|choices| choices.first())
        .and_then(|c| c.get("finish_reason"))
        .and_then(|v| v.as_str())
        .unwrap_or("end_turn");

    let stop_reason = if has_tool_use {
        "tool_use"
    } else {
        match finish_reason {
            "stop" | "end_turn" => "end_turn",
            "length" | "max_tokens" => "max_tokens",
            "tool_calls" => "tool_use",
            "content_filter" => "content_filter",
            other => other,
        }
    };
    out.insert(
        "stop_reason".to_string(),
        Value::String(stop_reason.to_string()),
    );

    // ── model ──────────────────────────────────────────────────────
    if let Some(model) = openai.get("model").and_then(|v| v.as_str()) {
        out.insert("model".to_string(), Value::String(model.to_string()));
    }

    // ── usage ──────────────────────────────────────────────────────
    let mut usage = Map::new();
    if let Some(openai_usage) = openai.get("usage") {
        if let Some(v) = openai_usage.get("input_tokens").or_else(|| openai_usage.get("prompt_tokens")).and_then(|v| v.as_u64()) {
            usage.insert("input_tokens".to_string(), Value::Number(v.into()));
        }
        if let Some(v) = openai_usage.get("output_tokens").or_else(|| openai_usage.get("completion_tokens")).and_then(|v| v.as_u64()) {
            usage.insert("output_tokens".to_string(), Value::Number(v.into()));
        }
    }
    out.insert("usage".to_string(), Value::Object(usage));

    Value::Object(out)
}

// ─── helpers ───────────────────────────────────────────────────────────

/// Copy a field from `src` to `out` if it exists.
fn copy_if_present(out: &mut Map<String, Value>, src: &Value, key: &str) {
    if let Some(val) = src.get(key) {
        out.insert(key.to_string(), val.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_anthropic_to_openai_basic() {
        let input = serde_json::json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "system": "You are a helpful assistant.",
            "messages": [
                {"role": "user", "content": "Hello!"},
                {"role": "assistant", "content": "Hi there!"}
            ]
        });

        let result = anthropic_to_openai(input);
        assert_eq!(result["model"], "claude-sonnet-4-20250514");
        assert_eq!(result["messages"][0]["role"], "system");
        assert_eq!(result["messages"][1]["role"], "user");
        assert_eq!(result["messages"][2]["role"], "assistant");
        assert_eq!(result["max_tokens"], 1024);
    }

    #[test]
    fn test_openai_to_anthropic_basic() {
        let input = serde_json::json!({
            "id": "chatcmpl-xxx",
            "model": "claude-sonnet-4-20250514",
            "choices": [{
                "message": {"role": "assistant", "content": "Hello! How can I help?"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 20
            }
        });

        let result = openai_to_anthropic(input);
        assert_eq!(result["type"], "message");
        assert_eq!(result["role"], "assistant");
        assert_eq!(result["content"][0]["text"], "Hello! How can I help?");
        assert_eq!(result["stop_reason"], "end_turn");
        assert_eq!(result["usage"]["input_tokens"], 10);
        assert_eq!(result["usage"]["output_tokens"], 20);
    }

    #[test]
    fn test_anthropic_content_blocks() {
        let input = serde_json::json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "Hello "},
                    {"type": "text", "text": "world!"}
                ]
            }]
        });

        let result = anthropic_to_openai(input);
        assert_eq!(result["messages"][0]["content"], "Hello \nworld!");
    }
}

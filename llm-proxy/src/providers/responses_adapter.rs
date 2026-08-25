use serde_json::{Value, json};
use std::collections::HashMap;

/// Convert between OpenAI Chat Completions and the Responses API at the
/// transport boundary.
///
/// Some upstream models are Responses-only and return HTTP 500 on
/// `/chat/completions`. Every upstream path in this proxy already speaks OpenAI
/// Chat Completions, so rather than teaching each client a new format we
/// convert Chat -> Responses on the way out and Responses -> Chat on the way
/// back. Everything downstream keeps thinking in Chat Completions.
///
/// These helpers are deliberately tolerant of missing fields: they never panic
/// and never `unwrap()` on external data — anything absent is skipped.

/// Convert a Chat Completions request body into a Responses request body.
pub fn chat_to_responses(chat: &Value) -> Value {
    let model = chat.get("model").cloned();

    let mut instructions_parts: Vec<String> = Vec::new();
    let mut input_items: Vec<Value> = Vec::new();

    if let Some(messages) = chat.get("messages").and_then(Value::as_array) {
        for m in messages {
            let role = m.get("role").and_then(Value::as_str).unwrap_or("");
            match role {
                // System/developer messages have no input item; fold their text
                // into the top-level instructions field.
                "system" | "developer" => {
                    if let Some(text) = extract_text(m.get("content")) {
                        if !text.is_empty() {
                            instructions_parts.push(text);
                        }
                    }
                }
                "user" => {
                    if let Some(parts) = content_to_input_parts(m.get("content")) {
                        let mut item = json!({"type": "message", "role": "user"});
                        item["content"] = Value::Array(parts);
                        input_items.push(item);
                    }
                }
                "assistant" => {
                    let text = extract_text(m.get("content"));
                    let tool_calls = m.get("tool_calls").and_then(Value::as_array);

                    let has_tool_calls = tool_calls
                        .as_ref()
                        .map(|calls| !calls.is_empty())
                        .unwrap_or(false);

                    // Emit the text item first (only when non-empty), then one
                    // function_call item per tool call.
                    if let Some(text) = &text {
                        if !text.is_empty() {
                            input_items.push(json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": text}]
                            }));
                        }
                    }
                    if has_tool_calls {
                        if let Some(calls) = tool_calls {
                            for tc in calls {
                                let name = tc
                                    .get("function")
                                    .and_then(|f| f.get("name"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                let call_id = tc.get("id").and_then(Value::as_str).unwrap_or("");
                                // arguments stays the raw JSON string.
                                let arguments = tc
                                    .get("function")
                                    .and_then(|f| f.get("arguments"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                input_items.push(json!({
                                    "type": "function_call",
                                    "call_id": call_id,
                                    "name": name,
                                    "arguments": arguments
                                }));
                            }
                        }
                    }
                }
                "tool" => {
                    let call_id = m.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
                    let output = extract_text(m.get("content")).unwrap_or_default();
                    input_items.push(json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": output
                    }));
                }
                _ => {}
            }
        }
    }

    let mut out = json!({"input": input_items, "store": false});
    if let Some(model) = model {
        out["model"] = model;
    }
    if !instructions_parts.is_empty() {
        out["instructions"] = Value::String(instructions_parts.join("\n\n"));
    }

    // Tools: unwrap from {"type":"function","function":{...}} to flat form.
    if let Some(tools) = chat.get("tools").and_then(Value::as_array) {
        let flat: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                let f = t.get("function")?;
                let name = f.get("name")?.as_str()?;
                let mut item = json!({"type": "function", "name": name, "strict": false});
                if let Some(d) = f.get("description") {
                    item["description"] = d.clone();
                }
                if let Some(p) = f.get("parameters") {
                    item["parameters"] = p.clone();
                }
                Some(item)
            })
            .collect();
        if !flat.is_empty() {
            out["tools"] = Value::Array(flat);
        }
    }

    // Copy through unchanged when present.
    for key in [
        "tool_choice",
        "parallel_tool_calls",
        "temperature",
        "top_p",
        "stream",
    ] {
        if let Some(v) = chat.get(key) {
            out[key] = v.clone();
        }
    }

    // max_completion_tokens wins when both present.
    let max_out = chat
        .get("max_completion_tokens")
        .or_else(|| chat.get("max_tokens"));
    if let Some(v) = max_out {
        out["max_output_tokens"] = v.clone();
    }

    out
}

/// Convert a Responses response body back into a non-streaming Chat
/// Completions object.
pub fn responses_to_chat(resp: &Value) -> Value {
    let id = resp.get("id").cloned().unwrap_or(Value::Null);
    let created = resp
        .get("created_at")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let model = resp.get("model").cloned().unwrap_or(Value::Null);

    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    if let Some(output) = resp.get("output").and_then(Value::as_array) {
        for item in output {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    if let Some(content) = item.get("content").and_then(Value::as_array) {
                        for part in content {
                            if part.get("type").and_then(Value::as_str) == Some("output_text") {
                                if let Some(t) = part.get("text").and_then(Value::as_str) {
                                    text.push_str(t);
                                }
                            }
                        }
                    }
                }
                Some("function_call") => {
                    let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
                    let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                    let arguments = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    tool_calls.push(json!({
                        "id": call_id,
                        "type": "function",
                        "function": {"name": name, "arguments": arguments}
                    }));
                }
                // Reasoning items carry only an opaque encrypted_content blob
                // with no Chat Completions equivalent (measured: summary is
                // always [] and content always null for this provider). Drop
                // it — do not "fix" this later.
                Some("reasoning") => {}
                _ => {}
            }
        }
    }

    let mut message = json!({
        "role": "assistant",
        "content": if text.is_empty() { Value::Null } else { Value::String(text) }
    });
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls.clone());
    }

    // finish_reason: any tool calls -> "tool_calls"; else incomplete due to
    // max_output_tokens -> "length"; else "stop".
    let finish_reason = if !tool_calls.is_empty() {
        "tool_calls"
    } else if resp.get("status").and_then(Value::as_str) == Some("incomplete")
        && resp
            .get("incomplete_details")
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            == Some("max_output_tokens")
    {
        "length"
    } else {
        "stop"
    };

    let mut out = json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason
        }]
    });

    // Usage: CRITICAL — copy input_tokens verbatim; do NOT subtract cached
    // tokens. In the Responses shape input_tokens already includes cached
    // tokens, exactly like Chat's prompt_tokens does. A sibling helper
    // (providers/codex/responses.rs::parse_usage) DOES subtract, because it
    // feeds a different consumer that wants fresh-input only. Copying that
    // here would double-subtract downstream.
    if let Some(usage) = resp.get("usage") {
        let mut u = json!({});
        if let Some(input) = usage.get("input_tokens") {
            u["prompt_tokens"] = input.clone();
        }
        if let Some(output) = usage.get("output_tokens") {
            u["completion_tokens"] = output.clone();
        }
        if let Some(total) = usage.get("total_tokens") {
            u["total_tokens"] = total.clone();
        }
        if let Some(cached) = usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
        {
            u["prompt_tokens_details"] = json!({"cached_tokens": cached});
        }
        if !u.is_null() {
            out["usage"] = u;
        }
    }

    out
}

/// Extract plain text from an OpenAI content field, which may be a string or
/// an array of parts. Returns `None` when there is no stringy text.
fn extract_text(content: Option<&Value>) -> Option<String> {
    let c = content?;
    if let Some(s) = c.as_str() {
        return Some(s.to_string());
    }
    if let Some(parts) = c.as_array() {
        let mut out = String::new();
        for part in parts {
            if part.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    out.push_str(t);
                }
            }
        }
        return Some(out);
    }
    None
}

/// Convert an OpenAI user content field into Responses `input_*` parts.
/// Returns `None` when there is nothing usable to emit.
fn content_to_input_parts(content: Option<&Value>) -> Option<Vec<Value>> {
    let c = content?;
    if let Some(s) = c.as_str() {
        return Some(vec![json!({"type": "input_text", "text": s})]);
    }
    if let Some(parts) = c.as_array() {
        let mut out: Vec<Value> = Vec::new();
        for part in parts {
            match part.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                        out.push(json!({"type": "input_text", "text": t}));
                    }
                }
                Some("image_url") => {
                    if let Some(url) = part
                        .get("image_url")
                        .and_then(|i| i.get("url"))
                        .and_then(Value::as_str)
                    {
                        out.push(json!({"type": "input_image", "image_url": url}));
                    }
                }
                _ => {}
            }
        }
        if out.is_empty() {
            return None;
        }
        return Some(out);
    }
    None
}

/// Re-frames a Responses SSE stream into OpenAI Chat Completions chunks.
///
/// Feed it one parsed Responses event at a time; it returns zero or more Chat
/// chunk objects to forward. The caller owns the `data: ` framing and the
/// terminal `[DONE]` line.
pub struct ResponsesToChatStream {
    /// Response id reported so far ("" before `response.created`/`in_progress`).
    id: String,
    /// Response model reported so far ("" before the creation events).
    model: String,
    /// Response created_at timestamp (0 before the creation events).
    created: u64,
    /// Next tool-call index to assign, starting at 0.
    next_tool_index: usize,
    /// Maps a Responses item_id to the tool index it was assigned.
    item_tool_index: HashMap<String, usize>,
    /// Whether any function_call item has been seen.
    saw_tool_call: bool,
}

impl Default for ResponsesToChatStream {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponsesToChatStream {
    pub fn new() -> Self {
        Self {
            id: String::new(),
            model: String::new(),
            created: 0,
            next_tool_index: 0,
            item_tool_index: HashMap::new(),
            saw_tool_call: false,
        }
    }

    /// Process one parsed Responses SSE event, returning zero or more Chat
    /// completion chunks to forward (the caller frames them as `data: ` lines).
    pub fn push_event(&mut self, event: &Value) -> Vec<Value> {
        let event_type = event.get("type").and_then(Value::as_str);
        match event_type {
            // Creation/progress events carry the response identity we echo on
            // every later chunk. Record it and emit nothing.
            Some("response.created") | Some("response.in_progress") => {
                self.record_response_identity(event);
                Vec::new()
            }
            Some("response.output_item.added") => self.on_output_item_added(event),
            Some("response.function_call_arguments.delta") => {
                self.on_arguments_delta(event)
            }
            Some("response.output_text.delta") => self.on_text_delta(event),
            Some("response.completed") | Some("response.incomplete") => {
                self.on_terminal(event)
            }
            // Everything else emits nothing. Reasoning events in particular
            // carry only an opaque encrypted_content blob with no Chat
            // Completions equivalent (measured: summary is always [] for this
            // provider), and the *_done / content_part events are just bookends
            // with no streaming payload.
            _ => Vec::new(),
        }
    }

    /// Record id/model/created from a response-level event when present.
    fn record_response_identity(&mut self, event: &Value) {
        if let Some(r) = event.get("response") {
            if let Some(id) = r.get("id").and_then(Value::as_str) {
                self.id = id.to_string();
            }
            if let Some(model) = r.get("model").and_then(Value::as_str) {
                self.model = model.to_string();
            }
            if let Some(created) = r.get("created_at").and_then(Value::as_u64) {
                self.created = created;
            }
        }
    }

    fn on_output_item_added(&mut self, event: &Value) -> Vec<Value> {
        let item_type = event
            .get("item")
            .and_then(|i| i.get("type"))
            .and_then(Value::as_str);
        if item_type != Some("function_call") {
            // Reasoning and message items have no streaming counterpart in the
            // added event (their payload streams in via deltas). Emit nothing.
            return Vec::new();
        }

        let item = match event.get("item") {
            Some(i) => i,
            None => return Vec::new(),
        };
        let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
        let index = self.next_tool_index;
        self.next_tool_index += 1;
        self.saw_tool_call = true;
        self.item_tool_index.insert(item_id.to_string(), index);

        let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
        let name = item.get("name").and_then(Value::as_str).unwrap_or("");

        vec![self.skeleton(json!([{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": index,
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": ""}
                }]
            },
            "finish_reason": Value::Null
        }]))]
    }

    fn on_arguments_delta(&mut self, event: &Value) -> Vec<Value> {
        let item_id = event.get("item_id").and_then(Value::as_str).unwrap_or("");
        // Default to index 0 when the item was never recorded (unknown id).
        let index = self
            .item_tool_index
            .get(item_id)
            .copied()
            .unwrap_or(0);
        let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
        // Continuation deltas deliberately omit id/name/type — the OpenAI
        // convention that the downstream accumulator expects.
        vec![self.skeleton(json!([{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": index,
                    "function": {"arguments": delta}
                }]
            },
            "finish_reason": Value::Null
        }]))]
    }

    fn on_text_delta(&mut self, event: &Value) -> Vec<Value> {
        let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
        vec![self.skeleton(json!([{
            "index": 0,
            "delta": {"content": delta},
            "finish_reason": Value::Null
        }]))]
    }

    fn on_terminal(&mut self, event: &Value) -> Vec<Value> {
        let mut chunks = Vec::new();

        // finish_reason: any tool call seen -> "tool_calls"; else incomplete
        // due to max_output_tokens -> "length"; else "stop".
        let reason = if self.saw_tool_call {
            "tool_calls"
        } else if event.get("response").and_then(|r| r.get("status")).and_then(Value::as_str)
            == Some("incomplete")
            && event
                .get("response")
                .and_then(|r| r.get("incomplete_details"))
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str)
                == Some("max_output_tokens")
        {
            "length"
        } else {
            "stop"
        };
        chunks.push(self.skeleton(json!([{
            "index": 0,
            "delta": {},
            "finish_reason": reason
        }])));

        // Usage chunk, built from response.usage when present.
        if let Some(usage) = event
            .get("response")
            .and_then(|r| r.get("usage"))
        {
            let mut u = json!({});
            if let Some(input) = usage.get("input_tokens") {
                // CRITICAL: copy input_tokens verbatim into prompt_tokens — do
                // NOT subtract cached tokens. Responses input_tokens already
                // includes them, exactly as Chat's prompt_tokens does.
                u["prompt_tokens"] = input.clone();
            }
            if let Some(output) = usage.get("output_tokens") {
                u["completion_tokens"] = output.clone();
            }
            if let Some(total) = usage.get("total_tokens") {
                u["total_tokens"] = total.clone();
            }
            if let Some(cached) = usage
                .get("input_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
            {
                u["prompt_tokens_details"] = json!({"cached_tokens": cached});
            }
            let mut usage_chunk = self.skeleton(Value::Array(Vec::new()));
            usage_chunk["usage"] = u;
            chunks.push(usage_chunk);
        }

        chunks
    }

    /// Build a chunk carrying the stream-reported identity so far. Every chunk
    /// the stream emits shares this skeleton; fields fall back to "" / 0 before
    /// the creation events arrive.
    fn skeleton(&self, choices: Value) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": choices
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(role: &str, content: &str) -> Value {
        json!({"role": role, "content": content})
    }

    #[test]
    fn system_and_developer_collapse_into_instructions() {
        let chat = json!({
            "model": "m",
            "messages": [
                m("system", "You are helpful."),
                m("developer", "Be concise."),
                m("user", "Hi")
            ]
        });
        let out = chat_to_responses(&chat);
        assert_eq!(out["instructions"], "You are helpful.\n\nBe concise.");
        // No input items come from the system/developer messages.
        let inputs = out["input"].as_array().unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0]["role"], "user");
    }

    #[test]
    fn user_string_becomes_input_text() {
        let chat = json!({"messages": [m("user", "Hello world")]});
        let out = chat_to_responses(&chat);
        let inputs = out["input"].as_array().unwrap();
        assert_eq!(inputs[0]["content"], json!([{"type": "input_text", "text": "Hello world"}]));
    }

    #[test]
    fn user_array_with_image_becomes_input_text_and_image() {
        let chat = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "Look at this"},
                    {"type": "image_url", "image_url": {"url": "https://x/img.png"}}
                ]
            }]
        });
        let out = chat_to_responses(&chat);
        let inputs = out["input"].as_array().unwrap();
        assert_eq!(
            inputs[0]["content"],
            json!([
                {"type": "input_text", "text": "Look at this"},
                {"type": "input_image", "image_url": "https://x/img.png"}
            ])
        );
    }

    #[test]
    fn assistant_tool_calls_become_function_call_items() {
        let chat = json!({
            "messages": [{
                "role": "assistant",
                "content": "Calling now",
                "tool_calls": [{
                    "id": "call_a",
                    "type": "function",
                    "function": {"name": "ping", "arguments": "{\"x\":1.0}"}
                }]
            }]
        });
        let out = chat_to_responses(&chat);
        let inputs = out["input"].as_array().unwrap();
        assert_eq!(inputs.len(), 2);
        // Text item first.
        assert_eq!(inputs[0]["content"][0]["type"], "output_text");
        assert_eq!(inputs[0]["content"][0]["text"], "Calling now");
        // arguments stays the original string, unparsed.
        let call = &inputs[1];
        assert_eq!(call["type"], "function_call");
        assert_eq!(call["call_id"], "call_a");
        assert_eq!(call["name"], "ping");
        assert_eq!(call["arguments"], "{\"x\":1.0}");
    }

    #[test]
    fn tool_message_becomes_function_call_output() {
        let chat = json!({
            "messages": [{
                "role": "tool",
                "tool_call_id": "call_xyz",
                "content": "pong"
            }]
        });
        let out = chat_to_responses(&chat);
        let inputs = out["input"].as_array().unwrap();
        assert_eq!(inputs[0]["type"], "function_call_output");
        assert_eq!(inputs[0]["call_id"], "call_xyz");
        assert_eq!(inputs[0]["output"], "pong");
    }

    #[test]
    fn tools_unwrapped_to_flat_form() {
        let chat = json!({
            "tools": [{
                "type": "function",
                "function": {
                    "name": "ping",
                    "description": "Pings.",
                    "parameters": {"type": "object", "properties": {}}
                }
            }]
        });
        let out = chat_to_responses(&chat);
        assert_eq!(
            out["tools"],
            json!([{
                "type": "function",
                "name": "ping",
                "description": "Pings.",
                "parameters": {"type": "object", "properties": {}},
                "strict": false
            }])
        );
    }

    #[test]
    fn max_tokens_maps_and_max_completion_wins() {
        let only_max = json!({"messages": [], "max_tokens": 100});
        assert_eq!(chat_to_responses(&only_max)["max_output_tokens"], 100);

        let both = json!({"messages": [], "max_tokens": 100, "max_completion_tokens": 250});
        assert_eq!(chat_to_responses(&both)["max_output_tokens"], 250);
    }

    #[test]
    fn sample_with_tool_call_produces_chat_shape() {
        let resp = json!({
            "id": "resp_6a8d", "object": "response", "created_at": 1787686987,
            "status": "completed", "model": "muse-spark-1.2-contributor",
            "output": [
                {"id": "rs_1", "type": "reasoning", "status": "completed",
                 "encrypted_content": "Q-PaDg...", "summary": []},
                {"id": "msg_1", "type": "message", "status": "completed", "role": "assistant",
                 "content": [{"type": "output_text", "text": "Calling ping with x=1 now.",
                              "annotations": []}]},
                {"id": "fc_1", "type": "function_call", "status": "completed", "name": "ping",
                 "call_id": "call_01a0", "arguments": "{\"x\":1.0}"}
            ],
            "usage": {"input_tokens": 539, "output_tokens": 267, "total_tokens": 806,
                      "input_tokens_details": {"cached_tokens": 497},
                      "output_tokens_details": {"reasoning_tokens": 200}}
        });
        let out = responses_to_chat(&resp);
        let msg = &out["choices"][0]["message"];
        assert_eq!(msg["content"], "Calling ping with x=1 now.");
        let calls = msg["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "call_01a0");
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn reasoning_item_is_dropped() {
        let resp = json!({
            "id": "r", "created_at": 0, "model": "m",
            "output": [
                {"type": "reasoning", "encrypted_content": "Q-PaDg..."}
            ]
        });
        let out = responses_to_chat(&resp);
        let content = out["choices"][0]["message"]["content"].clone();
        assert!(!content.to_string().contains("Q-PaDg"));
        assert!(out["choices"][0]["message"].get("tool_calls").is_none());
    }

    #[test]
    fn usage_maps_verbatim_without_subtracting_cached() {
        let resp = json!({
            "id": "r", "created_at": 0, "model": "m", "output": [],
            "usage": {"input_tokens": 539, "output_tokens": 267, "total_tokens": 806,
                      "input_tokens_details": {"cached_tokens": 497}}
        });
        let out = responses_to_chat(&resp);
        let u = &out["usage"];
        assert_eq!(u["prompt_tokens"], 539); // NOT 42 — cached tokens are included.
        assert_eq!(u["completion_tokens"], 267);
        assert_eq!(u["total_tokens"], 806);
        assert_eq!(u["prompt_tokens_details"]["cached_tokens"], 497);
    }

    #[test]
    fn incomplete_max_output_tokens_gives_length() {
        let resp = json!({
            "id": "r", "created_at": 0, "model": "m",
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": []
        });
        let out = responses_to_chat(&resp);
        assert_eq!(out["choices"][0]["finish_reason"], "length");
    }

    #[test]
    fn plain_text_gives_stop_and_no_tool_calls() {
        let resp = json!({
            "id": "r", "created_at": 0, "model": "m",
            "output": [
                {"type": "message", "content": [
                    {"type": "output_text", "text": "Just text."}
                ]}
            ]
        });
        let out = responses_to_chat(&resp);
        assert_eq!(out["choices"][0]["message"]["content"], "Just text.");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert!(out["choices"][0]["message"].get("tool_calls").is_none());
    }

    // --- ResponsesToChatStream streaming re-framer tests ---

    const USAGE: &str = r#"{
        "input_tokens": 539, "output_tokens": 267, "total_tokens": 806,
        "input_tokens_details": {"cached_tokens": 497}
    }"#;

    fn text_delta(delta: &str) -> Value {
        json!({"type": "response.output_text.delta", "delta": delta})
    }

    fn fn_call_added(item_id: &str, call_id: &str, name: &str) -> Value {
        json!({
            "type": "response.output_item.added",
            "item": {"id": item_id, "type": "function_call", "call_id": call_id, "name": name}
        })
    }

    fn args_delta(item_id: &str, delta: &str) -> Value {
        json!({"type": "response.function_call_arguments.delta", "item_id": item_id, "delta": delta})
    }

    fn completed(status: &str, usage: Option<&str>) -> Value {
        let mut e = json!({
            "type": "response.completed",
            "response": {"status": status, "id": "resp_1", "model": "m", "created_at": 1}
        });
        if let Some(u) = usage {
            e["response"]["usage"] = serde_json::from_str(u).unwrap();
        }
        e
    }

    #[test]
    fn text_deltas_stream_in_order() {
        let mut s = ResponsesToChatStream::new();
        let mut out = s.push_event(&text_delta("Hel"));
        out.extend(s.push_event(&text_delta("lo")));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["choices"][0]["delta"]["content"], "Hel");
        assert_eq!(out[1]["choices"][0]["delta"]["content"], "lo");
        // All chunks share the chunk skeleton.
        assert_eq!(out[0]["object"], "chat.completion.chunk");
    }

    #[test]
    fn function_call_opening_then_arguments_deltas() {
        let mut s = ResponsesToChatStream::new();
        let mut out = s.push_event(&fn_call_added("fc_1", "call_01a0", "ping"));
        out.extend(s.push_event(&args_delta("fc_1", "{\"x\":")));
        out.extend(s.push_event(&args_delta("fc_1", "1.0}")));
        out.extend(s.push_event(&args_delta("fc_1", "")));

        assert_eq!(out.len(), 4);
        // Opening chunk carries id + name + empty arguments, tool index 0.
        let open = &out[0]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(open["index"], 0);
        assert_eq!(open["id"], "call_01a0");
        assert_eq!(open["type"], "function");
        assert_eq!(open["function"]["name"], "ping");
        assert_eq!(open["function"]["arguments"], "");

        // Continuation deltas are arguments-only (no id/name/type), same index.
        for k in 1..4 {
            let tc = &out[k]["choices"][0]["delta"]["tool_calls"][0];
            assert_eq!(tc["index"], 0);
            assert!(tc.get("id").is_none());
            assert!(tc.get("type").is_none());
            assert!(tc.get("function").unwrap().get("name").is_none());
        }
        assert_eq!(out[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], "{\"x\":");
        assert_eq!(out[2]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], "1.0}");
        assert_eq!(out[3]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], "");
    }

    #[test]
    fn two_function_calls_get_distinct_indices() {
        let mut s = ResponsesToChatStream::new();
        let mut out = s.push_event(&fn_call_added("fc_a", "call_a", "ping"));
        out.extend(s.push_event(&fn_call_added("fc_b", "call_b", "echo")));
        out.extend(s.push_event(&args_delta("fc_a", "A")));
        out.extend(s.push_event(&args_delta("fc_b", "B")));

        assert_eq!(out.len(), 4);
        // Opening chunks keyed by item_id carry indices 0 and 1.
        assert_eq!(out[0]["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
        assert_eq!(out[1]["choices"][0]["delta"]["tool_calls"][0]["index"], 1);
        // Continuation deltas resolve to their owning item's index.
        assert_eq!(out[2]["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
        assert_eq!(out[3]["choices"][0]["delta"]["tool_calls"][0]["index"], 1);
    }

    #[test]
    fn completed_text_stream_yields_stop_then_usage() {
        let mut s = ResponsesToChatStream::new();
        s.push_event(&text_delta("hi"));
        let out = s.push_event(&completed("completed", Some(USAGE)));

        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["choices"][0]["finish_reason"], "stop");
        assert_eq!(out[0]["choices"][0]["delta"], json!({}));

        let usage_chunk = &out[1];
        assert_eq!(usage_chunk["choices"], json!([]));
        assert_eq!(usage_chunk["usage"]["prompt_tokens"], 539); // NOT 42 — cached NOT subtracted.
        assert_eq!(usage_chunk["usage"]["completion_tokens"], 267);
        assert_eq!(usage_chunk["usage"]["total_tokens"], 806);
        assert_eq!(usage_chunk["usage"]["prompt_tokens_details"]["cached_tokens"], 497);
    }

    #[test]
    fn completed_after_tool_call_gives_tool_calls() {
        let mut s = ResponsesToChatStream::new();
        s.push_event(&fn_call_added("fc_1", "call_01a0", "ping"));
        let out = s.push_event(&completed("completed", None));
        assert_eq!(out.len(), 1); // no usage -> only the finish chunk
        assert_eq!(out[0]["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn incomplete_max_output_tokens_gives_length_stream() {
        let mut s = ResponsesToChatStream::new();
        let mut e = json!({
            "type": "response.incomplete",
            "response": {"status": "incomplete", "id": "resp_1", "model": "m", "created_at": 1,
                         "incomplete_details": {"reason": "max_output_tokens"}}
        });
        e["response"]["usage"] = serde_json::from_str(USAGE).unwrap();
        let out = s.push_event(&e);
        assert_eq!(out[0]["choices"][0]["finish_reason"], "length");
    }

    #[test]
    fn reasoning_and_ping_emit_nothing() {
        let mut s = ResponsesToChatStream::new();
        let reasoning = json!({
            "type": "response.output_item.added",
            "item": {"id": "rs_1", "type": "reasoning", "encrypted_content": "Q-PaDg..."}
        });
        let ping = json!({"type": "ping"});
        let mut out = s.push_event(&reasoning);
        out.extend(s.push_event(&ping));
        assert!(out.is_empty());
    }

    #[test]
    fn response_created_carries_identity_into_later_chunks() {
        let mut s = ResponsesToChatStream::new();
        s.push_event(&json!({
            "type": "response.created",
            "response": {"id": "resp_9", "model": "muse-1", "created_at": 1787686987}
        }));
        let out = s.push_event(&text_delta("hi"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], "resp_9");
        assert_eq!(out[0]["model"], "muse-1");
        assert_eq!(out[0]["created"], 1787686987);
    }
}

//! Multimodal vision client for sending screenshot images to vision-capable
//! LLMs via the OpenAI-compatible `/chat/completions` endpoint, transparently
//! handling models that speak the Responses API by rewriting to `/responses`
//! and converting the request/reply bodies through the shared adapter.
//!
//! Request construction ([`build_multimodal_body`]) and response parsing
//! ([`parse_vision_response`]) are shared; [`analyze_png_blocking`] ties them
//! together over a blocking HTTP client.
//!
//! # Usage
//!
//! ```ignore
//! use llm_proxy::vision::analyze_png_blocking;
//! use llm_proxy::config::ResolvedModelEndpoint;
//!
//! let client = reqwest::blocking::Client::new();
//! let target = ResolvedModelEndpoint {
//!     provider: "my-provider".into(),
//!     model: "vision-model".into(),
//!     endpoint: "http://localhost:8080/v1/chat/completions".into(),
//!     api_key: "sk-...".into(),
//! };
//! let result = analyze_png_blocking(&client, &target, "Analyze this", &png_bytes)?;
//! ```

use base64::Engine;
use serde_json::Value;

use crate::config::ResolvedModelEndpoint;

/// Default prompt sent with the screenshot.
pub const DEFAULT_VISION_PROMPT: &str = "Analyze this screenshot and return the useful visible text. Preserve the \
     original language and structure. Return only the result, without commentary.";

// ── Shared request/response helpers ─────────────────────────────────────

/// Build the multimodal request body JSON.
pub fn build_multimodal_body(model: &str, prompt: &str, png: &[u8]) -> serde_json::Value {
    let b64 = base64::engine::general_purpose::STANDARD.encode(png);
    let data_url = format!("data:image/png;base64,{}", b64);
    serde_json::json!({
        "model": model,
        "stream": false,
        "messages": [
            {
                "role": "user",
                "content": [
                    {
                        "type": "text",
                        "text": prompt
                    },
                    {
                        "type": "image_url",
                        "image_url": {
                            "url": data_url
                        }
                    }
                ]
            }
        ]
    })
}

/// Parse the response body and extract the text content.
///
/// Handles both string content and text-block array formats.
/// Returns `None` if parsing fails or content is empty.
pub fn parse_vision_response(body: &[u8]) -> Result<String, String> {
    let resp: Value =
        serde_json::from_slice(body).map_err(|e| format!("Invalid JSON response: {e}"))?;

    let content = resp
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .ok_or_else(|| "Vision response missing choices[0].message.content".to_string())?;

    let text = if let Some(s) = content.as_str() {
        s.to_string()
    } else if let Some(blocks) = content.as_array() {
        let mut result = String::new();
        for block in blocks {
            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                result.push_str(t);
            }
        }
        result
    } else {
        return Err("Vision response content is neither a string nor text blocks".to_string());
    };

    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        return Err("Vision model returned no text".to_string());
    }

    Ok(trimmed)
}

// ── Request API ──────────────────────────────────────────────────────────

/// Resolve the HTTP route for a vision request.
///
/// The caller hands us a URL already ending in `/chat/completions`. A
/// Responses-only model needs the sibling `/responses` route instead.
/// For Chat Completions models the endpoint is returned unchanged.
fn vision_url(endpoint: &str, is_responses: bool) -> String {
    if is_responses {
        // Strip the trailing slash first: otherwise `.../chat/completions/`
        // fails to match the suffix and we would append to it instead of
        // replacing it.
        format!(
            "{}/responses",
            endpoint
                .trim_end_matches('/')
                .trim_end_matches("/chat/completions")
        )
    } else {
        endpoint.to_string()
    }
}

/// Send a non-streaming multimodal request over a blocking HTTP client.
///
/// Encodes the PNG as a `data:image/png;base64,` URL and sends it alongside
/// the text prompt in a single user message with two content blocks.
///
/// If the target model speaks the Responses API, the request body and reply
/// are converted through the shared adapter and the route is rewritten to
/// `/responses`; otherwise the request is sent byte-for-byte to
/// `/chat/completions` as before.
///
/// Returns the model's text response, trimmed.
pub fn analyze_png_blocking(
    client: &reqwest::blocking::Client,
    target: &ResolvedModelEndpoint,
    prompt: &str,
    png: &[u8],
) -> anyhow::Result<String> {
    let body = build_multimodal_body(&target.model, prompt, png);

    let models = crate::config::load_models().unwrap_or_default();
    let providers = crate::config::load_providers().unwrap_or_default();
    let is_responses = matches!(
        crate::config::resolve_upstream_api(&target.model, &models, &providers),
        crate::config::UpstreamApi::Responses
    );

    let url = vision_url(&target.endpoint, is_responses);
    let wire_body = if is_responses {
        crate::providers::responses_adapter::chat_to_responses(&body)
    } else {
        body
    };

    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", target.api_key))
        .header("Content-Type", "application/json")
        .json(&wire_body)
        .send()
        .map_err(|e| anyhow::anyhow!("Network error: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().unwrap_or_default();
        let excerpt: String = body_text.chars().take(500).collect();
        return Err(anyhow::anyhow!(
            "Vision request failed with HTTP {status}: {excerpt}"
        ));
    }

    let body_bytes = response
        .bytes()
        .map_err(|e| anyhow::anyhow!("Failed to read response: {e}"))?
        .to_vec();

    // A Responses reply must be reshaped into Chat shape before parsing, since
    // `parse_vision_response` reads `choices[0].message.content`. Never unwrap
    // on network data — fall through to the existing parser on parse failure.
    let body_bytes = if is_responses {
        match serde_json::from_slice::<Value>(&body_bytes) {
            Ok(reply) => {
                let chat = crate::providers::responses_adapter::responses_to_chat(&reply);
                match serde_json::to_vec(&chat) {
                    Ok(bytes) => bytes,
                    Err(_) => body_bytes,
                }
            }
            Err(_) => body_bytes,
        }
    } else {
        body_bytes
    };

    parse_vision_response(&body_bytes).map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vision_url_rewrites_chat_to_responses() {
        let url = vision_url("https://api.example.com/v1/chat/completions", true);
        assert_eq!(url, "https://api.example.com/v1/responses");
    }

    #[test]
    fn vision_url_rewrites_with_trailing_slash() {
        let url = vision_url("https://api.example.com/v1/chat/completions/", true);
        assert_eq!(url, "https://api.example.com/v1/responses");
    }

    #[test]
    fn vision_url_leaves_chat_completions_unchanged() {
        let endpoint = "https://api.example.com/v1/chat/completions";
        assert_eq!(vision_url(endpoint, false), endpoint);
    }

    #[test]
    fn chat_to_responses_round_trip_keeps_input_image() {
        let body = build_multimodal_body(
            "vision-model",
            "Analyze this",
            b"\x89PNG\r\n\x1a\nfake-png-bytes",
        );
        let responses = crate::providers::responses_adapter::chat_to_responses(&body);
        let has_input_image = contains_input_image(&responses);
        assert!(has_input_image, "Responses body should contain an input_image part");
    }

    /// Recursively walk the Responses body for any `{"type": "input_image"}` part.
    fn contains_input_image(value: &Value) -> bool {
        match value {
            Value::Array(items) => items.iter().any(contains_input_image),
            Value::Object(map) => {
                if map.get("type").and_then(Value::as_str) == Some("input_image") {
                    return true;
                }
                map.values().any(contains_input_image)
            }
            _ => false,
        }
    }
}

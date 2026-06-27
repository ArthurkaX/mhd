//! Multimodal vision client for sending screenshot images to vision-capable
//! LLMs via the OpenAI-compatible `/chat/completions` endpoint.
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

/// Send a non-streaming multimodal request over a blocking HTTP client.
///
/// Encodes the PNG as a `data:image/png;base64,` URL and sends it alongside
/// the text prompt in a single user message with two content blocks.
///
/// Returns the model's text response, trimmed.
pub fn analyze_png_blocking(
    client: &reqwest::blocking::Client,
    target: &ResolvedModelEndpoint,
    prompt: &str,
    png: &[u8],
) -> anyhow::Result<String> {
    let body = build_multimodal_body(&target.model, prompt, png);

    let response = client
        .post(&target.endpoint)
        .header("Authorization", format!("Bearer {}", target.api_key))
        .header("Content-Type", "application/json")
        .json(&body)
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

    parse_vision_response(&body_bytes).map_err(anyhow::Error::msg)
}

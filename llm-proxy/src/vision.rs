//! Vision (multimodal) client for sending screenshot images to vision-capable
//! LLMs via the OpenAI-compatible `/chat/completions` endpoint.
//!
//! # Usage
//!
//! ```ignore
//! use llm_proxy::vision::analyze_png;
//! use llm_proxy::config::ResolvedModelEndpoint;
//!
//! let client = reqwest::Client::new();
//! let target = ResolvedModelEndpoint {
//!     provider: "my-provider".into(),
//!     model: "vision-model".into(),
//!     endpoint: "http://localhost:8080/v1/chat/completions".into(),
//!     api_key: "sk-...".into(),
//! };
//! let result = analyze_png(&client, &target, "Analyze this", &png_bytes).await?;
//! ```

use std::time::Duration;

use base64::Engine;
use serde_json::Value;

use crate::config::ResolvedModelEndpoint;

/// Default prompt sent with the screenshot.
pub const DEFAULT_VISION_PROMPT: &str = "Analyze this screenshot and return the useful visible text. Preserve the \
     original language and structure. Return only the result, without commentary.";

/// Send a non-streaming multimodal request to the OpenAI-compatible endpoint.
///
/// Encodes the PNG as a `data:image/png;base64,` URL and sends it alongside
/// the text prompt in a single user message with two content blocks.
///
/// Returns the model's text response, trimmed.
///
/// # Errors
///
/// - Network/timeout errors from reqwest.
/// - HTTP non-200 responses (includes bounded body excerpt).
/// - Missing or empty `choices[0].message.content`.
/// - Malformed response JSON.
pub async fn analyze_png(
    client: &reqwest::Client,
    target: &ResolvedModelEndpoint,
    prompt: &str,
    png: &[u8],
) -> anyhow::Result<String> {
    // Base64-encode the PNG
    let b64 = base64::engine::general_purpose::STANDARD.encode(png);
    let data_url = format!("data:image/png;base64,{}", b64);

    // Build the multimodal request body
    let body = serde_json::json!({
        "model": target.model,
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
    });

    // Send the request
    let response = client
        .post(&target.endpoint)
        .header("Authorization", format!("Bearer {}", target.api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .timeout(Duration::from_secs(120))
        .send()
        .await?;

    // Check HTTP status
    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        let excerpt: String = body_text.chars().take(500).collect();
        return Err(anyhow::anyhow!(
            "Vision request failed with HTTP {status}: {excerpt}"
        ));
    }

    // Parse JSON response
    let resp: Value = response.json().await?;

    // Extract choices[0].message.content
    let content = resp
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .ok_or_else(|| anyhow::anyhow!("Vision response missing choices[0].message.content"))?;

    // Handle string content
    let text = if let Some(s) = content.as_str() {
        s.to_string()
    } else if let Some(blocks) = content.as_array() {
        // Handle content block array format
        let mut result = String::new();
        for block in blocks {
            if let Some(text_block) = block.get("text").and_then(|t| t.as_str()) {
                result.push_str(text_block);
            }
        }
        result
    } else {
        return Err(anyhow::anyhow!(
            "Vision response content is neither a string nor text blocks"
        ));
    };

    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        return Err(anyhow::anyhow!("Vision model returned no text"));
    }

    Ok(trimmed)
}

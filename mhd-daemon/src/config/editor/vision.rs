//! Vision connectivity tests for the Settings panel (LLM Proxy page).

use super::*;

/// Run a vision connectivity test using the selected model.
/// Sends a tiny embedded PNG to validate multimodal capability.
pub(crate) fn run_vision_test(
    vision_model: &Option<llm_proxy::config::ModelRef>,
    providers: &[UiProvider],
) -> Result<(), String> {
    let model_ref = vision_model
        .as_ref()
        .ok_or_else(|| "No model selected".to_string())?;

    let provider = providers
        .iter()
        .find(|p| p.name == model_ref.provider)
        .ok_or_else(|| format!("Provider '{}' not found", model_ref.provider))?;

    let secrets =
        llm_proxy::config::load_secrets().map_err(|_| "Could not load secrets".to_string())?;

    let api_key = secrets
        .provider_keys
        .get(&model_ref.provider)
        .filter(|k| !k.is_empty())
        .map(|s| s.as_str())
        .or(if !secrets.upstream_key.is_empty() {
            Some(secrets.upstream_key.as_str())
        } else {
            None
        })
        .ok_or_else(|| "API key is missing".to_string())?;

    // Use the application icon as the test image.
    let test_png = VISION_TEST_ICON_PNG;

    let endpoint = llm_proxy::config::normalize_vision_endpoint(&provider.endpoint);
    let seq = next_vision_seq();

    crate::core::llm_proxy::log_vision(
        llm_proxy::state::VisionTraceEntry {
            seq,
            provider: model_ref.provider.clone(),
            model: model_ref.model.clone(),
            endpoint: endpoint.clone(),
            status: None,
            error: None,
            duration_ms: 0,
        },
        Some(llm_proxy::LogEvent {
            seq,
            event_type: "VISION_TEST".to_string(),
            model: Some(model_ref.model.clone()),
            target: Some(model_ref.provider.clone()),
            target_model: Some(endpoint.clone()),
            ..Default::default()
        }),
    );

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("HTTP client: {e}"))?;

    let target = llm_proxy::ResolvedModelEndpoint {
        provider: model_ref.provider.clone(),
        model: model_ref.model.clone(),
        endpoint: endpoint.clone(),
        api_key: api_key.to_string(),
    };

    let result =
        llm_proxy::vision::analyze_png_blocking(&client, &target, VISION_TEST_PROMPT, test_png)
            .map_err(|e| format!("{e}"))?;

    crate::core::llm_proxy::log_vision(
        llm_proxy::state::VisionTraceEntry {
            seq,
            provider: model_ref.provider.clone(),
            model: model_ref.model.clone(),
            endpoint: endpoint.clone(),
            status: Some(200),
            error: None,
            duration_ms: 0,
        },
        Some(llm_proxy::LogEvent {
            seq,
            event_type: "VISION_TEST_OK".to_string(),
            model: Some(model_ref.model.clone()),
            target: Some(model_ref.provider.clone()),
            target_model: Some(endpoint.clone()),
            status: Some(200),
            ..Default::default()
        }),
    );

    eprintln!(
        "mhd: vision test succeeded for {} / {}: '{}'",
        model_ref.provider,
        model_ref.model,
        result.chars().take(60).collect::<String>()
    );
    Ok(())
}

/// Monotonic sequence number for vision test requests.
pub(crate) fn next_vision_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

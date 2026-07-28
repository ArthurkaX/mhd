//! Shared plumbing for the vision actions (`vision_screenshot`, `vision_snip`).
//!
//! Both controllers resolve the same vision model, send a PNG plus a prompt to
//! the same OpenAI-compatible endpoint, emit the same trace events, and copy
//! the result to the clipboard.  Only the *prompt source* and the *trace
//! label* differ, so everything else lives here.

use std::time::Duration;

use llm_proxy::LogEvent;
use llm_proxy::state::VisionTraceEntry;

use crate::app::{AppHandle, DaemonControl};
use crate::win32::clipboard;

/// Which vision action a request belongs to (selects the trace event prefix).
#[derive(Clone, Copy)]
pub enum VisionKind {
    Screenshot,
    Snip,
}

impl VisionKind {
    /// Trace event-type prefix, e.g. `VISION` → `VISION_START`.
    fn prefix(self) -> &'static str {
        match self {
            VisionKind::Screenshot => "VISION",
            VisionKind::Snip => "VISION_SNIP",
        }
    }

    /// Human-readable label for stderr diagnostics.
    fn label(self) -> &'static str {
        match self {
            VisionKind::Screenshot => "vision screenshot",
            VisionKind::Snip => "vision snip",
        }
    }
}

/// A resolved vision model endpoint and its credentials.
pub struct VisionEndpoint {
    pub provider: String,
    pub model: String,
    pub endpoint: String,
    pub api_key: String,
}

/// Resolve the configured vision model, provider endpoint, and API key.
///
/// The API key is the per-provider key if present, otherwise the global
/// upstream key.
pub fn load_vision_endpoint(handle: &AppHandle) -> Result<VisionEndpoint, String> {
    let cfg = handle.llm_proxy_config();

    let model_ref = cfg
        .vision_model
        .clone()
        .ok_or_else(|| "Configure a vision model first".to_string())?;

    let provider = cfg
        .providers
        .iter()
        .find(|p| p.name == model_ref.provider)
        .ok_or_else(|| "Vision model is unavailable".to_string())?;

    let secrets =
        llm_proxy::config::load_secrets().map_err(|_| "Could not load secrets".to_string())?;

    let api_key = secrets
        .provider_keys
        .get(&model_ref.provider)
        .filter(|k| !k.is_empty())
        .map(|s| s.as_str())
        .or_else(|| (!secrets.upstream_key.is_empty()).then_some(secrets.upstream_key.as_str()))
        .ok_or_else(|| "Vision provider API key is missing".to_string())?;

    Ok(VisionEndpoint {
        provider: model_ref.provider.clone(),
        model: model_ref.model.clone(),
        endpoint: provider.endpoint.clone(),
        api_key: api_key.to_string(),
    })
}

/// Monotonic, process-wide trace sequence number shared by all vision actions
/// so trace entries never collide.
fn next_vision_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Send `png` plus `prompt` to the resolved endpoint, emit trace events, and
/// copy the model's response to the clipboard.
///
/// Returns a concise, user-facing error message on any failure.
pub fn send_and_copy(
    handle: &AppHandle,
    kind: VisionKind,
    ep: &VisionEndpoint,
    prompt: &str,
    png: &[u8],
) -> Result<(), String> {
    let endpoint_url = llm_proxy::config::normalize_vision_endpoint(&ep.endpoint);
    let seq = next_vision_seq();

    log_event(kind, seq, ep, &endpoint_url, None, None, 0);

    if !handle.quiet() {
        eprintln!(
            "mhd: {}: sending to {} model {} at {}...",
            kind.label(),
            ep.provider,
            ep.model,
            endpoint_url
        );
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| {
            eprintln!("mhd: {}: HTTP client error: {e}", kind.label());
            "Vision request failed".to_string()
        })?;

    let target = llm_proxy::ResolvedModelEndpoint {
        provider: ep.provider.clone(),
        model: ep.model.clone(),
        endpoint: endpoint_url.clone(),
        api_key: ep.api_key.clone(),
    };

    let started = std::time::Instant::now();
    let text =
        llm_proxy::vision::analyze_png_blocking(&client, &target, prompt, png).map_err(|e| {
            let duration_ms = started.elapsed().as_millis() as u64;
            log_event(
                kind,
                seq,
                ep,
                &endpoint_url,
                None,
                Some(&e.to_string()),
                duration_ms,
            );
            eprintln!("mhd: {}: request failed: {e}", kind.label());
            format!("Vision request failed: {e}")
        })?;

    let duration_ms = started.elapsed().as_millis() as u64;
    log_event(kind, seq, ep, &endpoint_url, Some(200), None, duration_ms);

    clipboard::set_text(&text).map_err(|e| {
        eprintln!("mhd: {}: clipboard write failed: {e}", kind.label());
        "Could not copy the result".to_string()
    })?;

    if !handle.quiet() {
        let preview: String = text.chars().take(80).collect();
        eprintln!(
            "mhd: {}: result ({}) chars: \"{}\"",
            kind.label(),
            text.len(),
            preview
        );
    }

    Ok(())
}

/// Emit one vision trace entry + log event.  `status`/`error` select the phase:
/// `None`/`None` → START, `Some(200)`/`None` → OK, otherwise → ERR.
fn log_event(
    kind: VisionKind,
    seq: u64,
    ep: &VisionEndpoint,
    endpoint: &str,
    status: Option<u16>,
    error: Option<&str>,
    duration_ms: u64,
) {
    let suffix = match (status, error) {
        (None, None) => "START",
        (_, Some(_)) => "ERR",
        _ => "OK",
    };
    crate::core::llm_proxy::log_vision(
        VisionTraceEntry {
            seq,
            provider: ep.provider.clone(),
            model: ep.model.clone(),
            endpoint: endpoint.to_string(),
            status,
            error: error.map(str::to_string),
            duration_ms,
        },
        Some(LogEvent {
            seq,
            event_type: format!("{}_{}", kind.prefix(), suffix),
            model: Some(ep.model.clone()),
            target: Some(ep.provider.clone()),
            target_model: Some(endpoint.to_string()),
            duration_ms: (duration_ms > 0).then_some(duration_ms),
            error: error.map(str::to_string),
            status,
            ..Default::default()
        }),
    );
}

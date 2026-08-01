//! Safe, development-only capture endpoint for discovering the Codex wire contract.
//!
//! The probe binds to loopback, never writes request bodies, and never prints
//! credential values. It is intentionally not a proxy: it returns a distinctive
//! error so a Codex invocation terminates after its first request.
//!
//! Usage: `codex_wire_probe [port]` (default: 43111)

use axum::{Router, extract::Request, http::StatusCode, response::IntoResponse, routing::any};
use serde_json::{Map, Value, json};
use std::net::SocketAddr;

const MARKER: &str = "mhd_codex_probe";

fn present_header(headers: &axum::http::HeaderMap, name: &str) -> bool {
    headers.contains_key(name)
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn input_shape(value: &Value) -> Option<Value> {
    let array = value.get("input")?.as_array()?;
    let mut kinds = Vec::new();
    let mut type_values = Vec::new();
    for item in array.iter().take(64) {
        kinds.push(value_kind(item));
        if let Some(kind) = item.get("type").and_then(Value::as_str) {
            type_values.push(kind);
        }
    }
    Some(json!({
        "kind": "array",
        "len": array.len(),
        "item_kinds": kinds,
        "item_types": type_values,
    }))
}

fn json_shape(body: &[u8]) -> Value {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return json!({ "valid_json": false });
    };
    let Some(object) = value.as_object() else {
        return json!({ "valid_json": true, "top_level": value_kind(&value) });
    };

    let mut keys = object.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    let mut field_kinds = Map::new();
    for (key, value) in object {
        field_kinds.insert(key.clone(), Value::String(value_kind(value).to_string()));
    }

    let mut shape = json!({
        "valid_json": true,
        "top_level": "object",
        "keys": keys,
        "field_kinds": field_kinds,
    });
    if let Some(input) = input_shape(&value) {
        shape["input_shape"] = input;
    }
    shape
}

async fn capture(request: Request) -> impl IntoResponse {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let headers = request.headers().clone();
    let (parts, body) = request.into_parts();
    let body = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
        Ok(body) => body,
        Err(error) => {
            eprintln!("probe body read failed: {error}");
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                [("content-type", "application/json")],
                json!({ "error": MARKER, "message": "body read failed" }).to_string(),
            );
        }
    };

    let safe_headers = json!({
        "authorization": present_header(&headers, "authorization"),
        "cookie": present_header(&headers, "cookie"),
        "openai_beta": present_header(&headers, "openai-beta"),
        "openai_organization": present_header(&headers, "openai-organization"),
        "openai_project": present_header(&headers, "openai-project"),
        "content_type": headers.get("content-type").and_then(|v| v.to_str().ok()),
        "content_encoding": headers.get("content-encoding").and_then(|v| v.to_str().ok()),
        "transfer_encoding": headers.get("transfer-encoding").and_then(|v| v.to_str().ok()),
        "accept": headers.get("accept").and_then(|v| v.to_str().ok()),
        "user_agent_present": present_header(&headers, "user-agent"),
        "x_headers": headers.keys().filter(|name| name.as_str().starts_with("x-")).map(|name| name.as_str()).collect::<Vec<_>>(),
    });
    let report = json!({
        "method": method.as_str(),
        "path": uri.path(),
        "query_present": uri.query().is_some(),
        "body_bytes": body.len(),
        "headers": safe_headers,
        "json": json_shape(&body),
        "request_parts": { "version": format!("{:?}", parts.version) },
    });
    eprintln!("mhd_codex_probe request: {}", report);

    (
        StatusCode::IM_A_TEAPOT,
        [("content-type", "application/json")],
        json!({ "error": MARKER, "message": "wire probe captured the request", "report": report })
            .to_string(),
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port = std::env::args()
        .nth(1)
        .map(|value| value.parse::<u16>())
        .transpose()?
        .unwrap_or(43111);
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    eprintln!("Codex wire probe listening on http://{address}");
    eprintln!("No request bodies or credential values are persisted.");
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, Router::new().fallback(any(capture))).await?;
    Ok(())
}

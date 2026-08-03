use axum::{Json, http::StatusCode, response::IntoResponse};

use crate::state::ClientKind;

/// HTTP error returned by all proxy ingress handlers.
pub struct AppError {
    err: anyhow::Error,
    status: StatusCode,
    error_type: &'static str,
}

impl AppError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            err: anyhow::anyhow!(msg.into()),
            status: StatusCode::BAD_GATEWAY,
            error_type: "proxy_error",
        }
    }

    pub fn disabled_client(client: ClientKind) -> Self {
        Self {
            err: anyhow::anyhow!(format!(
                "mHD proxy is disabled for client '{}'",
                client.slot()
            )),
            status: StatusCode::SERVICE_UNAVAILABLE,
            error_type: "mhd_client_disabled",
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let msg = format!("{}", self.err);
        let body = serde_json::json!({
            "error": { "type": self.error_type, "message": msg }
        });
        (self.status, Json(body)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(err: E) -> Self {
        Self {
            err: err.into(),
            status: StatusCode::BAD_GATEWAY,
            error_type: "proxy_error",
        }
    }
}

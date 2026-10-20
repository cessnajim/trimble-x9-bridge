//! Errors as RFC 9457 problem documents with a machine-readable `code`.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub title: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, title: impl Into<String>) -> Self {
        ApiError { status, code, title: title.into(), detail: None, hint: None }
    }
    pub fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail = Some(d.into());
        self
    }
    pub fn hint(mut self, h: impl Into<String>) -> Self {
        self.hint = Some(h.into());
        self
    }
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "type": format!("https://x9-bridge.local/problems/{}", self.code),
            "title": self.title,
            "status": self.status.as_u16(),
            "code": self.code,
        });
        if let Some(d) = &self.detail {
            v["detail"] = json!(d);
        }
        if let Some(h) = &self.hint {
            v["hint"] = json!(h);
        }
        v
    }

    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", "Missing or unknown bearer token")
    }
    pub fn forbidden(what: &str) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", format!("Your role can't {what}"))
    }
    pub fn not_found(what: &str) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", format!("No such {what}"))
    }
    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", "Invalid request").detail(detail)
    }
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", "Conflict").detail(detail)
    }
    pub fn disconnected(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "scanner_disconnected", "The scanner is not reachable")
            .detail(detail)
            .hint("Plug in and power the X9; the bridge brings the USB link up.")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, [(header::CONTENT_TYPE, "application/problem+json")], self.to_json().to_string())
            .into_response()
    }
}

impl From<x9_client::X9Error> for ApiError {
    fn from(e: x9_client::X9Error) -> Self {
        match &e {
            x9_client::X9Error::Api { status: 409, .. } => {
                Self::new(StatusCode::CONFLICT, "scanner_busy", "The scanner is busy").detail(e.to_string())
            }
            x9_client::X9Error::Api { status, .. } => {
                Self::new(StatusCode::BAD_GATEWAY, "conflict", format!("The scanner refused the request ({status})"))
                    .detail(e.to_string())
            }
            _ => Self::disconnected(e.to_string()),
        }
    }
}

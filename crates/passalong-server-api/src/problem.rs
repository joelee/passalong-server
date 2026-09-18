//! Refusals, as `application/problem+json` (RFC 9457) with a stable `code`.
//! The client decides from the code alone whether to retry.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use passalong_server_core::clock::rfc3339;
use passalong_server_core::error::ApiError;
use serde_json::json;

/// An [`ApiError`] on its way out.
#[derive(Debug)]
pub struct Problem {
    error: ApiError,
    status: Option<StatusCode>,
    lease_expires_at: Option<u64>,
    retry_after: Option<u64>,
}

impl Problem {
    /// With the status the contract gives the code, unless told otherwise.
    pub fn status(mut self, status: StatusCode) -> Self {
        self.status = Some(status);
        self
    }

    /// For `LEASE_HELD`: when the lease ends, so the client knows how long
    /// to wait.
    pub fn lease_expires_at(mut self, at: Option<u64>) -> Self {
        self.lease_expires_at = at;
        self
    }

    /// For `RATE_LIMITED`: the seconds of its `Retry-After`.
    pub fn retry_after(mut self, seconds: u64) -> Self {
        self.retry_after = Some(seconds);
        self
    }
}

impl From<ApiError> for Problem {
    fn from(error: ApiError) -> Self {
        Self {
            error,
            status: None,
            lease_expires_at: None,
            retry_after: None,
        }
    }
}

/// What went wrong inside: no detail leaves the server.
pub fn internal() -> Response {
    let body = json!({ "code": "INTERNAL", "status": 500, "title": "the server failed; its log says how", "retryable": true });
    respond(StatusCode::INTERNAL_SERVER_ERROR, &body)
}

fn respond(status: StatusCode, body: &serde_json::Value) -> Response {
    let mut response = (status, body.to_string()).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    response
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let status = self
            .status
            .or_else(|| StatusCode::from_u16(self.error.http_status()).ok())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut body = json!({
            "code": self.error.code(),
            "status": status.as_u16(),
            "title": self.error.to_string(),
            "retryable": self.error.retryable(),
        });
        if let Some(at) = self.lease_expires_at {
            body["leaseExpiresAt"] = json!(rfc3339(at));
        }
        let mut response = respond(status, &body);
        if let Some(seconds) = self.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        if status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

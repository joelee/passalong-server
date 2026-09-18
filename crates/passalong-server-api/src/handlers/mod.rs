//! One handler per operation. Each takes what the contract says, calls the
//! core on the blocking pool, and answers what the contract says.

pub mod health;
pub mod items;
pub mod rewrite;
pub mod uploads;
pub mod viewer;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use passalong_server_core::error::ApiError;
use serde::de::DeserializeOwned;

use crate::problem::Problem;

/// The largest JSON body any operation needs, several times over. Only
/// content is large, and content is not JSON.
pub const JSON_CAP: usize = 256 * 1024;

/// Reads a JSON body of at most [`JSON_CAP`] bytes.
pub async fn json_body<T: DeserializeOwned>(body: Body) -> Result<T, Problem> {
    let bytes = axum::body::to_bytes(body, JSON_CAP).await.map_err(|_| {
        Problem::from(ApiError::InvalidRequest(format!(
            "a JSON body may be at most {JSON_CAP} bytes"
        )))
        .status(StatusCode::PAYLOAD_TOO_LARGE)
    })?;
    serde_json::from_slice(&bytes).map_err(|err| {
        Problem::from(ApiError::InvalidRequest(format!(
            "the body is not what this operation takes: {err}"
        )))
    })
}

/// What a handler answers.
pub type Answer = Result<Response, Problem>;

//! `healthz` and `readyz`: unauthenticated, and saying nothing.

use axum::extract::State;
use axum::http::StatusCode;
use passalong_server_core::error::ApiError;

use crate::state::AppState;

/// The process runs.
pub async fn healthz() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// The control database answers and the data directory is there. When not,
/// the server is failing closed, and an orchestrator should stop routing to
/// it, which is what 503 here is for; `healthz` stays 204, so that nothing
/// restarts a server whose disk is the problem.
pub async fn readyz(State(state): State<AppState>) -> StatusCode {
    let ready = state
        .blocking(|inner| {
            inner
                .control
                .ready()
                .map_err(|_| ApiError::ServiceUnavailable)?;
            let data = &inner.config.server.data_dir;
            if data.is_dir() {
                Ok(())
            } else {
                Err(ApiError::ServiceUnavailable)
            }
        })
        .await;
    if ready.is_ok() {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

//! What every handler can reach.

use std::sync::Arc;
use std::time::Duration;

use passalong_server_core::clock::Clock;
use passalong_server_core::config::Config;
use passalong_server_core::control::{Authenticated, Control};
use passalong_server_core::engines::{Engines, ServerEngine};
use passalong_server_core::error::ApiError;

use crate::problem::Problem;

/// How long a request waits for the control database before it fails
/// closed.
pub const BUSY: Duration = Duration::from_secs(10);

pub struct Inner {
    pub config: Config,
    pub control: Control,
    pub engines: Engines,
    pub clock: Arc<dyn Clock>,
    pub bridge: Arc<crate::bridge::BridgeStats>,
    /// Failed authentications, per client address.
    pub failures: crate::rate::FailureLimiter,
}

/// Shared by every request; cheap to clone.
#[derive(Clone)]
pub struct AppState(pub Arc<Inner>);

impl std::ops::Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl AppState {
    /// Runs `work`, which calls the synchronous core, on the blocking pool,
    /// so that a slow disk or a busy database never stalls other requests.
    pub async fn blocking<T, F>(&self, work: F) -> Result<T, ApiError>
    where
        T: Send + 'static,
        F: FnOnce(&Inner) -> Result<T, ApiError> + Send + 'static,
    {
        let state = self.clone();
        tokio::task::spawn_blocking(move || work(&state))
            .await
            .unwrap_or(Err(ApiError::ServiceUnavailable))
    }

    /// As [`Self::blocking`], with the caller's workspace open, for the
    /// operations an open rewrite can refuse. Such a refusal says when the
    /// session's lease ends: that is what the client tells its user, and
    /// after it `takeOverRewrite` is allowed.
    pub async fn in_workspace<T, F>(&self, who: &Authenticated, work: F) -> Result<T, Problem>
    where
        T: Send + 'static,
        F: FnOnce(&Inner, &ServerEngine) -> Result<T, ApiError> + Send + 'static,
    {
        let workspace = who.workspace.clone();
        self.blocking(move |inner| {
            let engine = inner.engines.get(&workspace)?;
            Ok(work(inner, &engine).map_err(|error| {
                let lease = match error {
                    ApiError::LeaseHeld | ApiError::RewriteInProgress => engine
                        .session_view()
                        .ok()
                        .flatten()
                        .map(|session| session.lease_expires_at),
                    _ => None,
                };
                Problem::from(error).lease_expires_at(lease)
            }))
        })
        .await?
    }
}

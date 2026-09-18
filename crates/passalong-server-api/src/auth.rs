//! The bearer layer. It runs before any handler and before any body is
//! read: a request without a valid key is answered from its headers alone.

use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use passalong_server_core::control::Authenticated;
use passalong_server_core::error::ApiError;

use crate::problem::Problem;
use crate::rate::client_address;
use crate::server::{KeySlot, PeerAddr};
use crate::state::AppState;

/// The key as the header carries it, or nothing: a missing header, another
/// scheme, and an empty token are all "no valid key".
fn bearer(request: &Request) -> Option<String> {
    let header = request.headers().get(AUTHORIZATION)?.to_str().ok()?;
    let token = header.strip_prefix("Bearer ")?.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

pub async fn layer(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    // An address that failed too often is not looked at, whatever it sends.
    let address = request
        .extensions()
        .get::<ConnectInfo<PeerAddr>>()
        .map(|ConnectInfo(peer)| {
            client_address(peer.0, request.headers(), state.config.listen.behind_proxy)
        });
    if let Some(wait) =
        address.and_then(|address| state.failures.limited(address, state.clock.now()))
    {
        return Problem::from(ApiError::RateLimited)
            .retry_after(wait)
            .into_response();
    }
    // A guess is what is counted: no key, or one that is not a key of this
    // server. An expired or revoked key was the right secret, and a client
    // that keeps sending one must not shut out its whole network.
    let guessed = || {
        if let Some(address) = address {
            state.failures.failed(address, state.clock.now());
        }
    };
    let Some(token) = bearer(&request) else {
        guessed();
        return Problem::from(ApiError::Unauthenticated).into_response();
    };
    // Against the database, every time: what the operator's CLI changed in
    // another process holds from this request on.
    let who: Result<Authenticated, ApiError> = state
        .blocking(move |inner| inner.control.authenticate(&token))
        .await;
    match who {
        Ok(who) => {
            if let Some(slot) = request.extensions().get::<KeySlot>() {
                slot.set(who.caller.key.as_str());
            }
            request.extensions_mut().insert(who);
            next.run(request).await
        }
        Err(refusal) => {
            if refusal == ApiError::Unauthenticated {
                guessed();
            }
            Problem::from(refusal).into_response()
        }
    }
}

//! Encryption and the rewrite session: `enableEncryption`, `freshStart`,
//! `replaceHeader`, and the six operations of a rewrite.
//!
//! The server compares key ids and keeps headers. It never sees a key, a
//! passphrase, or the six words, and no request here has a place for one.

use axum::Json;
use axum::body::Body;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use passalong_server_core::control::Authenticated;
use passalong_server_core::error::ApiError;
use passalong_server_core::ids::KeyId;
use passalong_server_core::rewrite::{RewriteKind, RewriteRequest};
use serde::Deserialize;
use serde_json::value::RawValue;

use super::viewer::{encryption_json, session_json};
use super::{Answer, json_body};
use crate::state::AppState;

/// A header: the client's document, kept as the bytes it came as. It must
/// be a JSON object, as the contract's `Opaque` is.
fn header_bytes(header: &RawValue) -> Result<Vec<u8>, ApiError> {
    if header.get().starts_with('{') {
        Ok(header.get().as_bytes().to_vec())
    } else {
        Err(ApiError::InvalidRequest(
            "a header is a JSON object".to_owned(),
        ))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Seal {
    key_id: String,
    header: Box<RawValue>,
}

/// `enableEncryption`.
pub async fn enable_encryption(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
    body: Body,
) -> Answer {
    let seal: Seal = json_body(body).await?;
    let caller = who.caller.clone();
    let view = state
        .in_workspace(&who, move |_, engine| {
            let header = header_bytes(&seal.header)?;
            engine.enable_encryption(&caller, KeyId::parse(&seal.key_id)?, header)
        })
        .await?;
    Ok(Json(encryption_json(&view)).into_response())
}

/// `freshStart`.
pub async fn fresh_start(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
    body: Body,
) -> Answer {
    let seal: Seal = json_body(body).await?;
    let caller = who.caller.clone();
    let view = state
        .in_workspace(&who, move |_, engine| {
            let header = header_bytes(&seal.header)?;
            engine.fresh_start(&caller, KeyId::parse(&seal.key_id)?, header)
        })
        .await?;
    Ok(Json(encryption_json(&view)).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplaceHeader {
    expected_key_id: String,
    header: Box<RawValue>,
}

/// `replaceHeader`.
pub async fn replace_header(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
    body: Body,
) -> Answer {
    let replace: ReplaceHeader = json_body(body).await?;
    let caller = who.caller.clone();
    let view = state
        .in_workspace(&who, move |_, engine| {
            let header = header_bytes(&replace.header)?;
            engine.replace_header(&caller, &KeyId::parse(&replace.expected_key_id)?, header)
        })
        .await?;
    Ok(Json(encryption_json(&view)).into_response())
}

/// `getRewrite`.
pub async fn get_rewrite(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
) -> Answer {
    let session = state
        .in_workspace(&who, |_, engine| {
            engine.session_view()?.ok_or(ApiError::NotFound)
        })
        .await?;
    Ok(Json(session_json(&session)).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Kind {
    Migrate,
    Rotate,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BeginRewrite {
    kind: Kind,
    #[serde(default)]
    expected_key_id: Option<String>,
    new_key_id: String,
    new_header: Box<RawValue>,
}

/// `beginRewrite`.
pub async fn begin_rewrite(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
    body: Body,
) -> Answer {
    let begin: BeginRewrite = json_body(body).await?;
    let caller = who.caller.clone();
    let session = state
        .in_workspace(&who, move |_, engine| {
            let request = RewriteRequest {
                kind: match begin.kind {
                    Kind::Migrate => RewriteKind::Migrate,
                    Kind::Rotate => RewriteKind::Rotate,
                },
                expected_key_id: begin
                    .expected_key_id
                    .as_deref()
                    .map(KeyId::parse)
                    .transpose()?,
                new_key_id: KeyId::parse(&begin.new_key_id)?,
                new_header: header_bytes(&begin.new_header)?,
            };
            engine.begin_rewrite(&caller, request)
        })
        .await?;
    Ok((StatusCode::CREATED, Json(session_json(&session))).into_response())
}

/// `heartbeatRewrite`.
pub async fn heartbeat_rewrite(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
) -> Answer {
    let caller = who.caller.clone();
    let session = state
        .in_workspace(&who, move |_, engine| engine.heartbeat_rewrite(&caller))
        .await?;
    Ok(Json(session_json(&session)).into_response())
}

/// `takeOverRewrite`.
pub async fn take_over_rewrite(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
) -> Answer {
    let caller = who.caller.clone();
    let session = state
        .in_workspace(&who, move |_, engine| engine.take_over_rewrite(&caller))
        .await?;
    Ok(Json(session_json(&session)).into_response())
}

/// Which rewrite is ended: a request that arrives late names another one,
/// and ends nothing.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EndRewrite {
    new_key_id: String,
}

/// `commitRewrite`.
pub async fn commit_rewrite(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
    body: Body,
) -> Answer {
    let end: EndRewrite = json_body(body).await?;
    let caller = who.caller.clone();
    let view = state
        .in_workspace(&who, move |_, engine| {
            engine.commit_rewrite(&caller, &KeyId::parse(&end.new_key_id)?)
        })
        .await?;
    Ok(Json(encryption_json(&view)).into_response())
}

/// `abortRewrite`.
pub async fn abort_rewrite(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
    body: Body,
) -> Answer {
    let end: EndRewrite = json_body(body).await?;
    let caller = who.caller.clone();
    let view = state
        .in_workspace(&who, move |_, engine| {
            engine.abort_rewrite(&caller, &KeyId::parse(&end.new_key_id)?)
        })
        .await?;
    Ok(Json(encryption_json(&view)).into_response())
}

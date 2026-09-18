//! Uploads: begin, content, commit, abort. Every one may be sent again; the
//! upload id is the idempotency key (`docs/api/rewrite-session.md`).

use axum::Json;
use axum::body::Body;
use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use passalong_server_core::clock::rfc3339;
use passalong_server_core::control::Authenticated;
use passalong_server_core::error::ApiError;
use passalong_server_core::ids::{ItemId, KeyId, UploadId};
use passalong_server_core::upload::{Begun, PutOutcome, UploadRequest, UploadState};
use passalong_server_core::workspace::Partition;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use super::items::{ItemOut, item_json};
use super::{Answer, json_body};
use crate::bridge::read_body;
use crate::problem::Problem;
use crate::state::{AppState, Inner};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BeginUpload {
    id: String,
    /// Kept as the bytes the client sent: on disk it is its `meta.json`.
    meta: Box<RawValue>,
    /// A byte count, as a string: item sizes are 64-bit, JSON numbers not.
    size: String,
    #[serde(default)]
    expected_key_id: Option<String>,
    #[serde(default)]
    in_rewrite: bool,
}

/// `{ item, created }`, with the item as it is stored now.
#[derive(Debug, Serialize)]
struct OutcomeOut {
    item: Option<ItemOut>,
    created: bool,
}

/// What `beginUpload` answers: a ticket, or the item that is there already.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum BegunOut {
    #[serde(rename_all = "camelCase")]
    Ticket {
        upload_id: String,
        expires_at: String,
    },
    Stored(OutcomeOut),
}

fn outcome_json(
    inner: &Inner,
    who: &Authenticated,
    outcome: &PutOutcome,
) -> Result<OutcomeOut, ApiError> {
    let engine = inner.engines.get(&who.workspace)?;
    // A rewrite may stage an item under its source's id, so the outcome
    // says which was meant. Once the session has ended, what was staged is
    // either the workspace's item or gone.
    let staged = if outcome.staged {
        engine.staged_item(&who.caller, &outcome.id).ok()
    } else {
        None
    };
    let item = match staged {
        Some(item) => Some(item),
        None => engine.item(Partition::Current, &outcome.id)?,
    };
    Ok(OutcomeOut {
        item: item.map(|item| item_json(&outcome.id, &item)),
        created: outcome.created,
    })
}

pub async fn begin_upload(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
    body: Body,
) -> Answer {
    let begin: BeginUpload = json_body(body).await?;
    let caller = who.clone();
    let answered = state
        .in_workspace(&caller, move |inner, engine| {
            let request = UploadRequest {
                id: ItemId::parse(&begin.id)?,
                meta: begin.meta.get().as_bytes().to_vec(),
                size: begin.size.parse().map_err(|_| {
                    ApiError::InvalidRequest(
                        "`size` is a byte count, as a string of digits".to_owned(),
                    )
                })?,
                expected_key_id: begin
                    .expected_key_id
                    .as_deref()
                    .map(KeyId::parse)
                    .transpose()?,
                in_rewrite: begin.in_rewrite,
            };
            Ok(match engine.begin_upload(&who.caller, request)? {
                Begun::Ticket(ticket) => (
                    StatusCode::CREATED,
                    BegunOut::Ticket {
                        upload_id: ticket.upload_id.as_str().to_owned(),
                        expires_at: rfc3339(ticket.expires_at),
                    },
                ),
                Begun::Stored(outcome) => (
                    StatusCode::OK,
                    BegunOut::Stored(outcome_json(inner, &who, &outcome)?),
                ),
            })
        })
        .await?;
    Ok((answered.0, Json(answered.1)).into_response())
}

pub async fn put_upload_content(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
    Path(upload): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Answer {
    let upload = UploadId::parse(&upload)?;
    let (caller, workspace, id) = (who.caller.clone(), who.workspace.clone(), upload.clone());
    let awaited = state
        .blocking(move |inner| inner.engines.get(&workspace)?.upload_state(&caller, &id))
        .await?;
    let UploadState::Awaiting(announced) = awaited else {
        // Committed already: acknowledged, and the content is not read.
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    // From the headers alone, before a byte of the content is read.
    let length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.parse::<u64>().ok());
    if length != Some(announced) {
        return Err(Problem::from(ApiError::ContentMismatch));
    }
    // The shelf reads on the blocking pool while the body arrives; no
    // workspace's lock is held meanwhile.
    let mut reader = read_body(body, state.bridge.clone());
    state
        .blocking(move |inner| {
            inner
                .engines
                .get(&who.workspace)?
                .put_upload_content(&who.caller, &upload, &mut reader)
        })
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub async fn commit_upload(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
    Path(upload): Path<String>,
) -> Answer {
    let upload = UploadId::parse(&upload)?;
    let caller = who.clone();
    let body = state
        .in_workspace(&caller, move |inner, engine| {
            let outcome = engine.commit_upload(&who.caller, &upload)?;
            outcome_json(inner, &who, &outcome)
        })
        .await?;
    Ok(Json(body).into_response())
}

pub async fn abort_upload(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
    Path(upload): Path<String>,
) -> Result<Response, Problem> {
    let upload = UploadId::parse(&upload)?;
    state
        .blocking(move |inner| {
            inner
                .engines
                .get(&who.workspace)?
                .abort_upload(&who.caller, &upload)
        })
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

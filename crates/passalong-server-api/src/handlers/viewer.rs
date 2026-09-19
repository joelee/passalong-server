//! `getViewer` and `getWorkspace`.

use axum::Json;
use axum::extract::{Extension, State};
use axum::response::IntoResponse;
use passalong_server_core::clock::rfc3339;
use passalong_server_core::control::Authenticated;
use passalong_server_core::error::ApiError;
use passalong_server_core::rewrite::RewriteKind;
use passalong_server_core::workspace::{
    EncryptionState, EncryptionView, Partition, Role, SessionView,
};
use serde::Serialize;
use serde_json::json;
use serde_json::value::RawValue;

use super::Answer;
use crate::state::AppState;

/// Where this server's source is. The AGPL (section 13) has whoever runs a
/// modified server for others offer them its source; a client can show this.
/// A modified version points it at its own: `repository` in `Cargo.toml`.
const SOURCE_URL: &str = env!("CARGO_PKG_REPOSITORY");

/// The API version this server speaks: the `/v1` of every path.
const API_VERSION: u32 = 1;

fn role(role: Role) -> &'static str {
    match role {
        Role::ReadWrite => "readWrite",
        Role::ReadOnly => "readOnly",
    }
}

pub async fn get_viewer(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
) -> Answer {
    let max = state.config.limits.max_item_bytes;
    Ok(Json(json!({
        "key": {
            "id": who.caller.key.as_str(),
            "label": who.label,
            "role": role(who.caller.role),
            "expiresAt": who.expires_at.map(rfc3339),
        },
        "server": {
            "version": env!("CARGO_PKG_VERSION"),
            "apiVersion": API_VERSION,
            "maxItemBytes": max.map(|bytes| bytes.to_string()),
            "sourceUrl": SOURCE_URL,
        },
    }))
    .into_response())
}

/// A document of the client's, given back as the bytes it came as.
fn raw(bytes: &[u8]) -> Option<Box<RawValue>> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| RawValue::from_string(text.to_owned()).ok())
}

/// `RewriteSession`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionOut {
    kind: &'static str,
    holder: String,
    lease_expires_at: String,
    new_key_id: String,
    /// The client's document, as the bytes `beginRewrite` brought.
    new_header: Option<Box<RawValue>>,
    staged_ids: Vec<String>,
    source_items: u64,
}

pub fn session_json(session: &SessionView) -> SessionOut {
    SessionOut {
        kind: match session.kind {
            RewriteKind::Migrate => "migrate",
            RewriteKind::Rotate => "rotate",
        },
        holder: session.holder.as_str().to_owned(),
        lease_expires_at: rfc3339(session.lease_expires_at),
        new_key_id: session.new_key_id.as_str().to_owned(),
        new_header: raw(&session.new_header),
        staged_ids: session.staged_ids.iter().map(ToString::to_string).collect(),
        source_items: session.source_items as u64,
    }
}

/// `Encryption`. The header is the client's document, stored as its bytes
/// and given back as the same bytes: a struct and not `json!`, which would
/// parse it and spell it anew.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EncryptionOut {
    state: &'static str,
    key_id: Option<String>,
    header: Option<Box<RawValue>>,
    rewrite: Option<SessionOut>,
}

pub fn encryption_json(view: &EncryptionView) -> EncryptionOut {
    EncryptionOut {
        state: match view.state {
            EncryptionState::Plaintext => "plaintext",
            EncryptionState::Sealed => "sealed",
            EncryptionState::Rewriting => "rewriting",
        },
        key_id: view.key_id.as_ref().map(|key| key.as_str().to_owned()),
        header: view.header.as_deref().and_then(raw),
        rewrite: view.rewrite.as_ref().map(session_json),
    }
}

/// `Workspace`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceOut {
    name: String,
    quota_bytes: String,
    used_bytes: String,
    item_count: usize,
    encryption: EncryptionOut,
}

/// `probeWrite`.
pub async fn probe_write(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
) -> Answer {
    state
        .blocking(move |inner| inner.engines.get(&who.workspace)?.probe_write(&who.caller))
        .await?;
    Ok(axum::http::StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CleanStaging {
    /// Accepted for the contract's sake; the server's own age rule decides.
    #[allow(dead_code)]
    older_than_secs: u64,
}

/// `cleanStaging`.
pub async fn clean_staging(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
    body: axum::body::Body,
) -> Answer {
    let _: CleanStaging = super::json_body(body).await?;
    let removed = state
        .blocking(move |inner| {
            if who.caller.role == Role::ReadOnly {
                return Err(ApiError::ForbiddenRole);
            }
            inner.engines.get(&who.workspace)?.clean_staging()
        })
        .await?;
    Ok(Json(json!({ "removed": removed })).into_response())
}

pub async fn get_workspace(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
) -> Answer {
    let body = state
        .blocking(move |inner| {
            let engine = inner.engines.get(&who.workspace)?;
            let info = inner
                .control
                .workspace_by_id(&who.workspace)
                .map_err(|_| ApiError::ServiceUnavailable)?
                .ok_or(ApiError::NotFound)?;
            Ok(WorkspaceOut {
                name: info.name,
                quota_bytes: engine.limits().quota_bytes.to_string(),
                used_bytes: engine.used_bytes()?.to_string(),
                item_count: engine.item_ids(Partition::Current)?.len(),
                encryption: encryption_json(&engine.encryption()?),
            })
        })
        .await?;
    Ok(Json(body).into_response())
}

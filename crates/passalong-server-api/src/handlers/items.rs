//! Items: list, look at, read, find, resolve, delete.

use axum::Json;
use axum::body::Body;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use passalong_server_core::clock::rfc3339;
use passalong_server_core::control::Authenticated;
use passalong_server_core::error::ApiError;
use passalong_server_core::ids::{ItemId, KeyId};
use passalong_server_core::shelf::StoredItem;
use passalong_server_core::workspace::{Partition, Resolved};
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::value::RawValue;

use super::Answer;
use crate::bridge::{one_range, stream_out};
use crate::problem::Problem;
use crate::state::AppState;

/// `partition`, as the contract spells it.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Which {
    #[default]
    Current,
    Plain,
    /// The open rewrite's next generation, for its holder alone.
    Staged,
}

#[derive(Debug, Default, Deserialize)]
pub struct Listing {
    after: Option<String>,
    #[serde(default)]
    partition: Which,
}

#[derive(Debug, Default, Deserialize)]
pub struct OnePartition {
    #[serde(default)]
    partition: Which,
    #[serde(rename = "expectedKeyId")]
    expected_key_id: Option<String>,
}

fn parse_after(after: Option<&str>) -> Result<Option<ItemId>, ApiError> {
    after.map(ItemId::parse).transpose()
}

/// The envelope. `meta` is the client's bytes, given back as they came;
/// nothing from inside it is ever a field of its own. It is a struct, and
/// not `json!`, because going through a `serde_json::Value` would parse
/// `meta` and write it out again in another spelling.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemOut {
    id: String,
    meta: Option<Box<RawValue>>,
    stored_bytes: String,
    received_at: String,
}

pub fn item_json(id: &ItemId, item: &StoredItem) -> ItemOut {
    let meta = std::str::from_utf8(&item.envelope.meta)
        .ok()
        .and_then(|text| RawValue::from_string(text.to_owned()).ok());
    ItemOut {
        id: id.as_str().to_owned(),
        meta,
        stored_bytes: item.size.to_string(),
        received_at: rfc3339(item.envelope.received_at),
    }
}

pub async fn list_items(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
    Query(query): Query<Listing>,
) -> Answer {
    let items = state
        .blocking(move |inner| {
            let engine = inner.engines.get(&who.workspace)?;
            let after = parse_after(query.after.as_deref())?;
            let listed = match query.partition {
                Which::Current => engine.items_after(Partition::Current, after.as_ref())?,
                Which::Plain => engine.items_after(Partition::Plain, after.as_ref())?,
                Which::Staged => {
                    let mut found = Vec::new();
                    for id in engine.staged_item_ids(&who.caller)? {
                        if after.as_ref().is_none_or(|after| &id > after) {
                            let item = engine.staged_item(&who.caller, &id)?;
                            found.push((id, item));
                        }
                    }
                    found
                }
            };
            Ok(listed
                .iter()
                .map(|(id, item)| item_json(id, item))
                .collect::<Vec<_>>())
        })
        .await?;
    Ok(Json(items).into_response())
}

pub async fn list_item_ids(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
    Query(query): Query<Listing>,
) -> Answer {
    let ids = state
        .blocking(move |inner| {
            let engine = inner.engines.get(&who.workspace)?;
            let after = parse_after(query.after.as_deref())?;
            let mut ids = match query.partition {
                Which::Current => engine.item_ids_after(Partition::Current, after.as_ref())?,
                Which::Plain => engine.item_ids_after(Partition::Plain, after.as_ref())?,
                Which::Staged => engine.staged_item_ids(&who.caller)?,
            };
            if let (Which::Staged, Some(after)) = (query.partition, &after) {
                ids.retain(|id| id > after);
            }
            Ok(ids.iter().map(ToString::to_string).collect::<Vec<_>>())
        })
        .await?;
    Ok(Json(ids).into_response())
}

pub async fn get_item(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
    Path(id): Path<String>,
    Query(query): Query<OnePartition>,
) -> Answer {
    let body = state
        .blocking(move |inner| {
            let engine = inner.engines.get(&who.workspace)?;
            let id = ItemId::parse(&id)?;
            let item = match query.partition {
                Which::Current => engine.item(Partition::Current, &id)?,
                Which::Plain => engine.item(Partition::Plain, &id)?,
                Which::Staged => Some(engine.staged_item(&who.caller, &id)?),
            };
            item.map(|item| item_json(&id, &item))
                .ok_or(ApiError::NotFound)
        })
        .await?;
    Ok(Json(body).into_response())
}

pub async fn find_by_content_key(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
    Path(content_key): Path<String>,
) -> Answer {
    let body = state
        .blocking(move |inner| {
            let well_formed = content_key.len() == 12
                && content_key
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
            if !well_formed {
                return Err(ApiError::InvalidId("not a content key".to_owned()));
            }
            let engine = inner.engines.get(&who.workspace)?;
            let id = engine
                .find_by_content_key(&content_key)?
                .ok_or(ApiError::NotFound)?;
            let item = engine
                .item(Partition::Current, &id)?
                .ok_or(ApiError::NotFound)?;
            Ok(item_json(&id, &item))
        })
        .await?;
    Ok(Json(body).into_response())
}

#[derive(Debug, Deserialize)]
pub struct Typed {
    input: String,
}

pub async fn resolve_item(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
    Query(typed): Query<Typed>,
) -> Answer {
    let body = state
        .blocking(move |inner| {
            Ok(match inner.engines.get(&who.workspace)?.resolve_item(&typed.input)? {
                Resolved::Id(id) => json!({ "result": "resolved", "id": id.as_str() }),
                Resolved::Ambiguous(candidates) => json!({
                    "result": "ambiguous",
                    "candidates": candidates.iter().map(ToString::to_string).collect::<Vec<_>>(),
                }),
                Resolved::NotFound => json!({ "result": "notFound" }),
                Resolved::InvalidPrefix => json!({ "result": "invalidPrefix" }),
            })
        })
        .await?;
    Ok(Json(body).into_response())
}

pub async fn delete_item(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
    Path(id): Path<String>,
    Query(query): Query<OnePartition>,
) -> Answer {
    let caller = who.clone();
    let body = state
        .in_workspace(&caller, move |_, engine| {
            let id = ItemId::parse(&id)?;
            let partition = match query.partition {
                Which::Current => Partition::Current,
                Which::Plain => Partition::Plain,
                Which::Staged => {
                    return Err(ApiError::InvalidRequest(
                        "a staged item is dropped with its rewrite, not deleted".to_owned(),
                    ));
                }
            };
            let expected = query
                .expected_key_id
                .as_deref()
                .map(KeyId::parse)
                .transpose()?;
            let gone = engine.delete_item(&who.caller, partition, &id, expected.as_ref())?;
            Ok(item_json(&id, &gone))
        })
        .await?;
    Ok(Json(body).into_response())
}

pub async fn get_item_content(
    State(state): State<AppState>,
    Extension(who): Extension<Authenticated>,
    Path(id): Path<String>,
    Query(query): Query<OnePartition>,
    headers: HeaderMap,
) -> Answer {
    let range_header = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    // The reader is opened under the rules and read after: no workspace's
    // lock is held while content leaves.
    let opened = state
        .blocking(move |inner| {
            let engine = inner.engines.get(&who.workspace)?;
            let id = ItemId::parse(&id)?;
            let item = match query.partition {
                Which::Current => engine.item(Partition::Current, &id)?,
                Which::Plain => engine.item(Partition::Plain, &id)?,
                Which::Staged => Some(engine.staged_item(&who.caller, &id)?),
            }
            .ok_or(ApiError::NotFound)?;
            let Ok(range) = one_range(range_header.as_deref(), item.size) else {
                return Ok(Err(item.size));
            };
            let (first, last) = range.unwrap_or((0, item.size.saturating_sub(1)));
            let content = match query.partition {
                Which::Current => engine.item_content_from(Partition::Current, &id, first)?,
                Which::Plain => engine.item_content_from(Partition::Plain, &id, first)?,
                Which::Staged => Some(engine.staged_item_content_from(&who.caller, &id, first)?),
            }
            .ok_or(ApiError::NotFound)?;
            let length = if item.size == 0 { 0 } else { last - first + 1 };
            Ok(Ok((content, item.size, range, length)))
        })
        .await?;
    let (content, size, range, length) = match opened {
        Ok(opened) => opened,
        Err(size) => {
            let mut response = Problem::from(ApiError::InvalidRequest(
                "the range lies beyond the content".to_owned(),
            ))
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .into_response();
            set(
                &mut response,
                header::CONTENT_RANGE,
                &format!("bytes */{size}"),
            );
            return Ok(response);
        }
    };
    let mut response = Response::new(if length == 0 {
        Body::empty()
    } else {
        stream_out(content, length, state.bridge.clone())
    });
    set(
        &mut response,
        header::CONTENT_TYPE,
        "application/octet-stream",
    );
    set(&mut response, header::CONTENT_LENGTH, &length.to_string());
    set(&mut response, header::ACCEPT_RANGES, "bytes");
    if let Some((first, last)) = range {
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
        set(
            &mut response,
            header::CONTENT_RANGE,
            &format!("bytes {first}-{last}/{size}"),
        );
    }
    Ok(response)
}

fn set(response: &mut Response, name: header::HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        response.headers_mut().insert(name, value);
    }
}

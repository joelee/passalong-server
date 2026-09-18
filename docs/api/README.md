# API (draft)

> **Draft for discussion**, from
> [IDEA-00001](../ideas/00001-HTTPS_Server_Backend-r03.md). Once the routes
> exist in code, `openapi.json` in this folder is exported from them and a
> test fails when the two differ.

This folder is the contract between passalong-server and the passalong
client. The client implements it from these documents alone; it shares no
code with this repository.

The API is REST + JSON over HTTPS. Item content travels as plain byte
streams on the same API, never inside JSON.

## Rules

- Everything is under `/v1`. A change a released client cannot handle goes
  under `/v2`; additions that old clients can ignore do not. Clients ignore
  fields they do not know.
- Every request carries `Authorization: Bearer pal_<key id>_<secret>`. The
  key selects the workspace; no request names one. The key is checked
  before any body is read.
- **The envelope.** The server never interprets `meta` and never exposes a
  field taken from it, in plaintext and encrypted workspaces alike (see
  [the envelope](../architecture.md#the-envelope)). A test asserts that no
  response type carries such a field.
- JSON bodies are small and capped at a fixed size. Only content streams
  are large, and those go to disk as they arrive.
- Errors are `application/problem+json` (RFC 9457) with a stable `code`.
  The client decides from the code alone whether to retry.
- Every request is safe to repeat; see [Replays](#replays). A client that
  lost an answer sends the request again.
- The server fails closed: when it cannot read its control database it
  answers 503 `SERVICE_UNAVAILABLE` to everything that needs a key, and
  never authenticates from memory.
- A client may send `X-Request-Id`; the server logs it and returns it.

## Routes

Each route's name is its OpenAPI `operationId`, which the other documents
use.

### Server and workspace

| Operation | Route | Notes |
|---|---|---|
| `getViewer` | `GET /v1/viewer` | The key (id, label, role, `expiresAt`) and the server (`version`, `apiVersion`, `maxItemBytes`, which is `null` for no limit) |
| `getWorkspace` | `GET /v1/workspace` | Name, quota, bytes used, item count, and `encryption`: state, `keyId`, the `header`, and the rewrite session if one is open |
| `probeWrite` | `POST /v1/workspace/probe` | Checks the role, the rewrite state, and that staging is writable |
| `cleanStaging` | `POST /v1/workspace/clean-staging` | `{ "olderThanSecs": … }`; answers the number removed |
| `healthz` | `GET /healthz` | Liveness: the process runs. Unauthenticated, no detail |
| `readyz` | `GET /readyz` | Readiness: the control database and the data directory are usable. Fails, while `healthz` stays up, when the server is failing closed |

### Items

`partition` is `current` (the default) or `plain`, the items kept from
before a fresh start.

| Operation | Route | Notes |
|---|---|---|
| `listItems` | `GET /v1/items?after=<id>&partition=` | Envelopes with `meta`, newest first, in one response |
| `listItemIds` | `GET /v1/item-ids?after=<id>&partition=` | Ids alone, from the in-memory index; what pull mode polls |
| `getItem` | `GET /v1/items/{id}?partition=` | One envelope |
| `getItemContent` | `GET /v1/items/{id}/content?partition=` | The bytes; supports `Range` |
| `findByContentKey` | `GET /v1/items?contentKey=<12 hex>` | The oldest item with that content key, or an empty list |
| `resolveItem` | `GET /v1/items/resolve?input=<text>` | The client's prefix rules, on ids alone, so it works for sealed workspaces. Answers `resolved`, `ambiguous` with candidates, `notFound`, or `invalidPrefix` |
| `deleteItem` | `DELETE /v1/items/{id}?partition=&expectedKeyId=` | Answers the deleted envelope |

An envelope:

```json
{
  "id": "6aa52107-2cf24dba5fb0",
  "meta": { "schema": 2, "id": "6aa52107-2cf24dba5fb0", "nonce": "…", "sealed": "…" },
  "storedBytes": "5",
  "receivedAt": "2026-09-12T09:53:12Z"
}
```

`meta` is the client's `meta.json`, byte for byte: schema 1 in a plaintext
workspace, the sealed schema 2 in an encrypted one. Byte counts are strings,
because JSON numbers lose precision above 2^53 and item sizes are `u64`.

### Uploads

| Operation | Route | Notes |
|---|---|---|
| `beginUpload` | `POST /v1/uploads` | `{ id, meta, size, expectedKeyId, rewriteSessionId }`. 201 with an upload ticket (`uploadId`, `expiresAt`), or 200 with a put outcome when the content is already stored |
| `putUploadContent` | `PUT /v1/uploads/{uploadId}/content` | The bytes; `Content-Length` must equal `size`. 204 |
| `commitUpload` | `POST /v1/uploads/{uploadId}/commit` | Verifies, deduplicates, publishes. Answers the put outcome: `{ item, created }` |
| `abortUpload` | `DELETE /v1/uploads/{uploadId}` | 204 |

`expectedKeyId` is `null` for a plaintext workspace. `rewriteSessionId` is
set only while staging a rewrite's next generation.

### Encryption

| Operation | Route | Notes |
|---|---|---|
| `enableEncryption` | `PUT /v1/workspace/encryption` | `{ header, keyId }`; only while the workspace is empty |
| `replaceHeader` | `PUT /v1/workspace/encryption/header` | `{ expectedKeyId, header }`; the change of words: same data key, no item touched |
| `beginRewrite` | `POST /v1/rewrite` | `{ kind, expectedKeyId, newKeyId, newHeader }`; `kind` is `migrate`, `freshStart`, or `rotate`. Takes the lease; other writers now get `REWRITE_IN_PROGRESS` |
| `getRewrite` | `GET /v1/rewrite` | The open session: kind, holder, `leaseExpiresAt`, and the ids already staged |
| `heartbeatRewrite` | `POST /v1/rewrite/heartbeat` | Extends the lease |
| `takeOverRewrite` | `POST /v1/rewrite/take-over` | Only once the lease expired; for `encrypt --recover` from another device |
| `commitRewrite` | `POST /v1/rewrite/commit` | One transaction: generation pointer, header, key id |
| `abortRewrite` | `POST /v1/rewrite/abort` | Drops the staged generation |

A workspace has at most one rewrite session, so the routes name none.

## Replays

A proposal, to be confirmed by the spike of IDEA-00001 §14
(IDEA-00001-R03-MAJ-01 and -MED-05). The upload id is the idempotency key of
an upload; a rewrite needs none, because a workspace has one session.

| Request, sent again | Answer |
|---|---|
| `beginUpload`, same API key, same `id` and `size`, ticket still live | The same ticket; quota is not reserved twice |
| `beginUpload` for content already stored | The put outcome with `created: false`, as the first time |
| `putUploadContent` before commit | The staging file is truncated and written again |
| `putUploadContent` after commit | 204; the body is discarded |
| `commitUpload` after it succeeded, within the retention window | The **same** put outcome, including the original `created` value |
| `commitUpload` after the retention window | `NOT_FOUND`; the client settles it with `getItem`: it proposed the id, so it knows it |
| `abortUpload` of something already gone | 204 |
| `deleteItem` of something already gone | `NOT_FOUND`, which the client treats as done |
| `enableEncryption`, `replaceHeader` whose effect is already in place | The current encryption state, not an error |
| `beginRewrite` by the key that holds the session, same arguments | The live session |
| `commitRewrite` after it succeeded | The current encryption state, recognised by `newKeyId`. This is the replay that matters most: a client that lost the answer must learn that the commit happened, not conclude that it has to start again |
| `abortRewrite` with no session open | The current encryption state |

A committed upload leaves a tombstone holding its outcome for the retention
window, which is at least `staging.max_age_hours`.

## Error codes

| Code | HTTP | Meaning | Client retries |
|---|---|---|---|
| `UNAUTHENTICATED` | 401 | No key, or an unknown one | No |
| `KEY_EXPIRED`, `KEY_REVOKED` | 401 | As named; `serve` stops and says so | No |
| `FORBIDDEN_ROLE` | 403 | A read-only key tried to write | No |
| `KEY_ID_MISMATCH` | 409 | The workspace's data key changed; the device must `encrypt --join` | No, until joined |
| `REWRITE_IN_PROGRESS` | 409 | Encryption is being changed | Yes, with back-off |
| `LEASE_HELD` | 409 | `takeOverRewrite` before the lease expired | Yes, after `leaseExpiresAt` |
| `QUOTA_EXCEEDED` | 413 | The workspace is full | No |
| `ITEM_TOO_LARGE` | 413 | Larger than `maxItemBytes`; a limit the other backends do not have, so the client names it and the limit in its message | No |
| `CONTENT_MISMATCH` | 422 | Size or, in a plaintext workspace, SHA-256 differs from `meta` | No |
| `NOT_FOUND` | 404 | As named | No |
| `INVALID_ID`, `INVALID_REQUEST` | 400 | As named | No |
| `RATE_LIMITED` | 429 | Too many failed authentications | Yes, after `Retry-After` |
| `SERVICE_UNAVAILABLE` | 503 | The server is failing closed, for example because its control database is locked or damaged | Yes, with back-off |

## Later

- `GET /v1/events`: a server-sent-events stream of item changes, so pull
  mode need not poll (IDEA-00001-R03-LOW-01).

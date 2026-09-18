# API (draft)

> **Draft for discussion**, from
> [IDEA-00001](../ideas/00001-HTTPS_Server_Backend-r02.md). Once the schema
> exists in code, `schema.graphql` in this folder is exported from it and a
> test fails when the two differ.

This folder is the contract between passalong-server and the passalong
client. The client implements it from these documents alone; it shares no
code with this repository.

## Rules

- Everything is under `/v1`. A change a released client cannot handle goes
  under `/v2`; additions that old clients can ignore do not.
- Every request carries `Authorization: Bearer pal_<key id>_<secret>`. The
  key selects the workspace; no request names one.
- The server never interprets `meta` (see
  [the envelope](../architecture.md#the-envelope)).
- Errors carry a stable `code` in `extensions`. The client decides from the
  code alone whether to retry.
- Every mutation and every `PUT` is safe to repeat; see
  [Replays](#replays). A client that lost an answer sends the request again.
- The server fails closed: when it cannot read its control database it
  answers 503 `SERVICE_UNAVAILABLE` to everything that needs a key, and
  never authenticates from memory.

## Endpoints

| Endpoint | Purpose |
|---|---|
| `POST /v1/graphql` | Control and metadata |
| `PUT /v1/uploads/{uploadId}` | Stream one item's content; `Content-Length` must equal the size given to `beginUpload` |
| `GET /v1/items/{id}/content` | Stream content; supports `Range`; `?partition=plain` for the plain partition |
| `GET /healthz` | Liveness: the process runs. Unauthenticated, no detail |
| `GET /readyz` | Readiness: the control database and the data directory are usable. Fails, while `/healthz` stays up, when the server is failing closed |

## Schema sketch

```graphql
scalar DateTime
scalar ItemId        # "<8 hex>-<12 hex>", parsed strictly
scalar KeyId         # id of a workspace's data key, hex
scalar Json          # an opaque document; the server does not look inside

enum Role { READ_WRITE READ_ONLY }
enum EncryptionState { PLAINTEXT SEALED REWRITING }
enum RewriteKind { MIGRATE FRESH_START ROTATE }
enum Partition { CURRENT PLAIN }

type Query {
  viewer: Viewer!
  workspace: Workspace!
  items(after: ItemId, partition: Partition = CURRENT): [Item!]!   # newest first
  itemIds(after: ItemId, partition: Partition = CURRENT): [ItemId!]!
  item(id: ItemId!, partition: Partition = CURRENT): Item
  itemByContentKey(key: String!): Item
  resolveItem(input: String!): ResolveResult!
}

type Viewer {
  key: ApiKeyInfo!
  server: ServerInfo!
}
type ApiKeyInfo { id: String! label: String role: Role! expiresAt: DateTime }
# maxItemBytes is null when the operator set no limit. The client checks it
# before it uploads, because the ssh and local backends have no limit.
type ServerInfo { version: String! apiVersion: Int! maxItemBytes: String }

type Workspace {
  name: String!
  quotaBytes: String!
  usedBytes: String!
  itemCount: Int!
  encryption: Encryption!
}
type Encryption {
  state: EncryptionState!
  keyId: KeyId
  header: Json            # the wrapped data key, for `encrypt --join`
  rewrite: RewriteSession
}
type RewriteSession {
  id: ID!
  kind: RewriteKind!
  startedByKey: String!
  leaseExpiresAt: DateTime!
  stagedIds: [ItemId!]!
}

type Item {
  id: ItemId!
  meta: Json!
  storedBytes: String!
  receivedAt: DateTime!
}

union ResolveResult = Resolved | Ambiguous | NotFound | InvalidPrefix
type Resolved { id: ItemId! }
type Ambiguous { candidates: [ItemId!]! }
type NotFound { input: String! }
type InvalidPrefix { input: String! }

type Mutation {
  beginUpload(input: BeginUploadInput!): BeginUploadResult!
  commitUpload(uploadId: ID!): PutOutcome!
  abortUpload(uploadId: ID!): Boolean!
  deleteItem(id: ItemId!, expectedKeyId: KeyId, partition: Partition = CURRENT): Item!
  cleanStaging(olderThanSecs: Int!): Int!
  probeWrite: Boolean!

  enableEncryption(header: Json!, keyId: KeyId!): Encryption!
  replaceHeader(expectedKeyId: KeyId!, header: Json!): Encryption!
  beginRewrite(kind: RewriteKind!, expectedKeyId: KeyId, newKeyId: KeyId!, newHeader: Json!): RewriteSession!
  heartbeatRewrite(sessionId: ID!): RewriteSession!
  takeOverRewrite(sessionId: ID!): RewriteSession!       # only once the lease expired
  commitRewrite(sessionId: ID!): Encryption!
  abortRewrite(sessionId: ID!): Encryption!
}

input BeginUploadInput {
  id: ItemId!
  meta: Json!
  size: String!
  expectedKeyId: KeyId        # null for a plaintext workspace
  rewriteSessionId: ID        # set while staging a rewrite's next generation
}
union BeginUploadResult = UploadTicket | PutOutcome
type UploadTicket { uploadId: ID! expiresAt: DateTime! }
type PutOutcome { item: Item! created: Boolean! }

# Later: type Subscription { itemsChanged: ItemsChanged! }
```

Byte counts are strings because GraphQL's `Int` is 32 bits.

## Replays

A proposal, to be confirmed by the spike of IDEA-00001 §14
(IDEA-00001-R02-MED-07). The upload id is the idempotency key.

| Request, sent again | Answer |
|---|---|
| `beginUpload`, same API key, same `id` and `size`, ticket still live | The same `UploadTicket`; quota is not reserved twice |
| `beginUpload` for content already stored | `PutOutcome` with `created: false`, as the first time |
| `PUT /v1/uploads/{uploadId}` before commit | The staging file is truncated and written again |
| `PUT /v1/uploads/{uploadId}` after commit | 204; the body is discarded |
| `commitUpload` after it succeeded, within the retention window | The **same** `PutOutcome`, including the original `created` value |
| `commitUpload` after the retention window | `NOT_FOUND`; the client settles it with `item(id:)` |
| `abortUpload`, `deleteItem` of something already gone | Success for `abortUpload`; `NOT_FOUND` for `deleteItem`, which the client treats as done |
| `enableEncryption`, `replaceHeader`, `commitRewrite`, `abortRewrite` whose effect is already in place | The current `Encryption`, not an error |
| `beginRewrite` by the key that holds the session, same arguments | The live `RewriteSession` |

A committed upload leaves a tombstone holding its outcome for the retention
window, which is at least `staging.max_age_hours`. A client that lost the
upload id altogether asks `item(id:)`: it proposed the id, so it knows it.

## Error codes

| Code | Meaning | Client retries |
|---|---|---|
| `UNAUTHENTICATED` | No key, or an unknown one | No |
| `KEY_EXPIRED`, `KEY_REVOKED` | As named; `serve` stops and says so | No |
| `FORBIDDEN_ROLE` | A read-only key tried to write | No |
| `KEY_ID_MISMATCH` | The workspace's data key changed; the device must `encrypt --join` | No, until joined |
| `REWRITE_IN_PROGRESS` | Encryption is being changed | Yes, with back-off |
| `QUOTA_EXCEEDED` | The workspace is full | No |
| `ITEM_TOO_LARGE` | Larger than `ServerInfo.maxItemBytes`; a limit the other backends do not have, so the client names it and the limit in its message | No |
| `CONTENT_MISMATCH` | Size or, in a plaintext workspace, SHA-256 differs from `meta` | No |
| `NOT_FOUND`, `INVALID_ID` | As named | No |
| `RATE_LIMITED` | Too many failed authentications | Yes, after `Retry-After` |
| `QUERY_TOO_COMPLEX` | Depth or complexity limit | No |
| `SERVICE_UNAVAILABLE` | HTTP 503: the server is failing closed, for example because its control database is locked or damaged | Yes, with back-off |

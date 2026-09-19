# Hand-over notes: the `https` backend of passalong v0.3.0

For whoever plans and builds the passalong client's `https` backend, human
or agent, working in the client repository
(<https://github.com/joelee/passalong>, Apache-2.0). Written on 2026-09-19 by
the agent that built the server, from the server's side of the contract.
It says where things are, what is proven, what is not, and what the server
needs the client to get right.

## Read these, in this order

| Document | For |
|---|---|
| [`api/openapi.json`](api/openapi.json) | **The contract.** 26 operations. A test compares it with the server's routes, operation for operation; another calls every one |
| [`api/README.md`](api/README.md) | The same in prose: rules, routes, replays, error codes and whether each is worth retrying |
| [`api/client-encryption-mapping.md`](api/client-encryption-mapping.md) | **Your refactoring, already worked out**: every function of the client's `encryption/` mapped to the API, the `EncryptionAdmin` and `Rewrite` traits sketched, and a six-point list of what v0.3.0 has to do. Start your plan from its last section |
| [`api/rewrite-session.md`](api/rewrite-session.md) | Migrating and rotating: leases, take-over, and a table of what every possible crash leaves and how to recover |
| [`encryption.md`](encryption.md) | The flows as sequence diagrams, and what a compromised server can and cannot do |
| [`usage.md`](usage.md), [`../deploy/docker/README.md`](../deploy/docker/README.md) | Running a server to test against |

## The licence boundary: read, do not copy

The server is `AGPL-3.0-or-later`; the client is Apache-2.0. **No code from
this repository may go into the client**, not a function, not a test, not
`crates/passalong-server-api/src/client.rs`, which looks tempting and is only
the server's own test client. AGPL code cannot enter an Apache-2.0 work.
What you may use freely is the *contract*: the OpenAPI document, the
behaviour these pages describe, the error codes. Implement from the
documents. (The other direction is open: the server may take client code,
with the client's `NOTICE`. It has taken none: its item-id and key-id types
are its own few lines.)

The client must not depend on a crate of this repository either, and none
is published.

## What is proven, and what is not

Proven, on the server's side, by tests you can read:

- Every operation answers as `openapi.json` says, and a request sent twice
  does what `x-passalong-replay` says.
- A model test interrupts and replays a scripted client at every step of
  upload, migrate, and rotate, 210 variants, and checks that no item is lost
  or duplicated and that the workspace is never half under one key.
- The process is killed at every boundary between the database and the
  files, a few hundred times a run, and recovers.
- `meta` and the header come back **byte for byte** as sent.
- 64 MiB passes through in both directions with at most 512 KiB held.

**Not proven: that a client can actually be written against it.** No
passalong client has ever spoken to this server. The contract was designed
from a close reading of the client's `Store` trait and encryption code, and
the only things that have used it are the server's own test client and
`curl`. Expect to find places where the contract is awkward or wrong for
you. When you do:

- say so, in the server repository, before working around it. The contract
  is `1.0.0-draft` and **no release has been tagged**, precisely so that it
  can still change without a `/v2`;
- an *additive* change is cheap. One has happened already:
  `server.sourceUrl` in `getViewer`;
- once the server's v0.1.0 is tagged, a change a released client cannot
  handle needs `/v2`.

## What the server needs the client to get right

These are not checked by the server, and cannot be. They are yours.

1. **A device that holds a key never sends plaintext to that workspace,
   whatever the server says.** The server tells a device whether a workspace
   is plaintext or sealed, and under which key id. A compromised or
   misconfigured server can say "plaintext" of a sealed workspace. If the
   device has a key file for it, that answer is an error to stop on, not an
   instruction. [`encryption.md`](encryption.md#if-the-server-is-compromised)
   promises users this. For a *new* device, ask the person whether the
   workspace is encrypted rather than trusting `getWorkspace` alone, or at
   least say clearly what the server claimed before the first send.
2. **`expectedKeyId` on every write**: `beginUpload`, `deleteItem`, and the
   encryption calls. `null` or absent means "I believe this workspace is
   plaintext". The server compares; it cannot do more. The key id is the
   client's existing one, 16 hex digits of
   `HKDF-SHA256(data key, "passalong key id v1")`. Do not invent another.
3. **Send `meta` as the bytes of your `meta.json`**, schema 1 or sealed
   schema 2, and expect the same bytes back. The server never parses it,
   with one exception:
4. **In a plaintext workspace the server checks content at the commit**:
   `meta.sha256` is the content's SHA-256, `meta.size` is its size and equals
   the announced `size`, and the id's content key is the first 12 hex digits
   of that hash. Anything else is `CONTENT_MISMATCH` (422). This is the
   client's own item schema, so an honest client passes without trying. In
   a sealed workspace nothing is looked at.
5. **Verify what you read.** The server authenticates nothing about content:
   GCM tags, the binding of sealed `meta` to the item id, and the content
   hash are the client's to check, as it does today for `ssh` and `local`.
   A server can roll a workspace back or withhold items and nothing in the
   protocol shows it; do not build anything that assumes a listing is
   complete or current.
6. **Read back before `commitRewrite`.** `partition=staged` exists so that
   the holder can fetch what it staged and compare before the old
   generation is deleted. After the commit there is no way back.

## Behaviour worth knowing before you design

- **One API key is one workspace and one role.** No request names a
  workspace. `getViewer` tells you the key's id, label, role, and expiry:
  show the expiry; keys expire after 90 days unless made otherwise.
- **Uploads are three calls**: `beginUpload` (answers a ticket, or 200 with
  the stored item when that content is there already, in which case send
  nothing), `putUploadContent`, `commitUpload`. The upload id is the
  idempotency key. A broken `putUploadContent` is simply sent again, whole,
  to the same ticket. There is no resumable upload in v1.
- **Quota is reserved at `beginUpload`**, and `maxItemBytes` (from
  `getViewer`; `null` means none) is a limit your other backends do not
  have: check it before sending, and name it in the message.
- **Sizes are strings** in JSON (64-bit); times are RFC 3339 UTC.
- **Downloads take `Range`**, one range, so an interrupted download resumes.
- **Every request may be repeated**, and a lost answer is handled by
  repeating. Read "Replays" before writing retry logic; most of what a
  client usually does to be safe is unnecessary here, and some of it
  (inventing a new upload for a retry) is harmful.
- **Errors are `application/problem+json`** with a stable `code` and a
  `retryable` flag. Decide from `code`. `LEASE_HELD` and
  `REWRITE_IN_PROGRESS` carry `leaseExpiresAt`: tell the user how long.
- **`serve` must stop for good** on `KEY_EXPIRED` and `KEY_REVOKED`, wait on
  `REWRITE_IN_PROGRESS` and `SERVICE_UNAVAILABLE`, and on `KEY_ID_MISMATCH`
  keep the file and ask for `encrypt --join`.
- **`RATE_LIMITED` (429) with `Retry-After`** answers an address that sent
  ten wrong keys in a minute, right keys included. A client that retries a
  401 in a loop locks out every device behind the same router. A 401 is
  never worth retrying.
- **A long rewrite needs `heartbeatRewrite`**, well inside the lease (ten
  minutes by default), or another device may take the session over.
  `REWRITE_ENDED` means: that rewrite was aborted; begin again with a new key.
- **Polling**: there is no event stream in v1. `listItemIds?after=<id>` is
  one cheap query; pull mode polls it.
- **HTTP/1.1 only.** One connection per request is fine; keep-alive works.

## TLS, as the client must handle it

- The server speaks TLS 1.2 and 1.3 itself, or plain HTTP behind a proxy.
- **`tls_pin`**: `sha256/` plus the base64 of the SHA-256 of the
  certificate's SubjectPublicKeyInfo (the form of `curl --pinnedpubkey`,
  which writes it with two slashes). `passalong-server tls fingerprint`
  prints it. With a pin, trust that public key and **nothing else**: no
  authority, no name, no date, but do verify the handshake's signature, or
  the pin proves nothing. Without a pin, verify normally.
- **No switch that turns verification off**, on either side. The server has
  none and documents that the client has none. A self-signed server is
  handled by the pin.
- A pin names the *key*. A Let's Encrypt renewal keeps it only with
  `--reuse-key`, and a publicly trusted certificate needs no pin. The server
  tells operators both; the client's documentation should agree.

## A server to build against

Nothing is published yet, so build it:

```text
git clone <the server repository> && cd passalong-server
cargo build --release --locked -p passalong-server
export HOME="$(mktemp -d)"          # keep it away from your own configuration
P=target/release/passalong-server
$P init
$P tls self-signed --host localhost --ip 127.0.0.1     # prints the pin
$P workspace create home
$P key create --workspace home --label dev             # prints the key, once
$P serve
```

`init` as a normal user writes below `$HOME`; the port is 8443. `key create
--read-only` makes a key for testing refusals; `key revoke` and
`key extend` take effect at the server's next request, with no restart.
`passalong-server rewrite abort <workspace>` ends a session a test left open.
[`usage.md`](usage.md#a-session-with-curl) has a whole session with `curl`.

For your integration tests, prefer starting this binary on a free port per
test over mocking the API: the replay and rewrite behaviour is the hard
part, and a mock will agree with whatever you believe.

## Open on the server's side

- **Not released.** v0.1.0 is built and its release notes are drafted; the
  owner has held the tag, and the first client work is a good reason to.
- **`just test-client`** is a reserved recipe name in the server's
  `justfile`: a released client run against this server in CI. It wants a
  client release to exist.
- **`GET /v1/events`**, server-sent events so that pull mode need not poll,
  is in the server's backlog. Say if polling proves too slow or too chatty.
- The backlog also holds `workspace import` and `export` for stores of the
  `ssh` and `local` backends, whose files are byte-identical to the server's
  by design. That is how an existing store would move to a server.

# The client's encryption code, mapped onto the API

What replaces each public function of the passalong client's encryption
code when the store is a passalong-server workspace, and what the client
must refactor for it. Written by PLAN-00001 (REQ-06) to test assumption
A-02 of IDEA-00001: that the client's encryption admin can be separated
from `RemoteFs`.

- **Client read:** `../passalong` at tag `v0.2.1`, commit
  `785a276061c4b47400b16ff89982470d5489a65e`, read only.
- **Modules:** `crates/passalong-core/src/encryption/{admin, rewrite,
  header_change, journal, open}.rs`, and the functions `header.rs` exports
  beside them.
- **Public** means `pub` and re-exported by `encryption/mod.rs`. Items that
  are `pub(super)` or `pub(crate)` are the journal's and the header
  change's internals; they have no counterpart because the mechanism they
  serve has none (see "What disappears").

## Verdict

**The refactor does not reach into `crypto/`.** Every primitive the HTTPS
backend needs is already public there: `Sealer::seal_meta`,
`Sealer::content_sealer`, `Sealer::content_key`, `Sealer::open_meta`,
`Sealer::open_item_content`, `sealed_len`, `wrap`, `unwrap`,
`DataKey::generate`, `DataKey::key_id`. None changes.

What is private today, and must be shared, is one level up: the **file
formats** in `store/fs_store.rs`, namely `SealedMetaFile` and
`SealedMetaBody`, and the loop that frames sealed content. The server
stores `meta.json` and `content` byte for byte as the client writes them,
so `HttpStore` has to produce the same bytes `FsStore` does. That is a move
of code within `passalong-core`, not a change of cryptography.

The orchestration maps cleanly. The client's rewrite engine, `run` in
`rewrite.rs`, is already "for each item of a *source store*: compute its id
in the *target store*, skip it if it exists, otherwise `import` it; then
verify each; then install the header". Replace "target store" by "the open
rewrite's next generation" and "install" by `commitRewrite`, and it is the
server's protocol. r03's success threshold is met.

## Function by function

`FS` marks what the function does to the store through `RemoteFs` today.

### `admin.rs`

| Function | What it does (FS) | Over HTTPS | `crypto/` changes |
|---|---|---|---|
| `inspect` | Classifies the layout: plain, encrypted, rewriting, or broken, by looking for `items/`, the stop file, `encryption/header.json`, `.rewrite/` | `getWorkspace`: `encryption.state`, `keyId`, `rewrite`. There is no "broken": the state is one database row, not a layout that can arrive in pieces | None |
| `set_up` | Refuses unless plaintext and empty; journalled header change: stop file, then header | `enableEncryption` | None: `DataKey::generate`, `wrap` as today |
| `fresh_start` | Journalled header change: `items/` moves to `plain/items/`, stop file, header | `freshStart`, one atomic call | None |
| `join` | Reads the header, unwraps it with the words | `getWorkspace`, then `unwrap` locally | None |
| `change_words` | `join`, wraps the same key anew, journalled header swap (two renames, between which the store has no header) | `replaceHeader` with `expectedKeyId`. No headerless moment | None |
| `leftovers`, `remove_leftovers` | Counts and removes the plaintext `tmp/` and journals staged for locks never taken | **Not needed.** Staging is the server's, reclaimed by its janitor; `cleanStaging` is the explicit form. Plaintext staging cannot survive into a sealed workspace: an upload begun under no key is refused at its commit | None |
| `plain_store` | The `plain/items/` folder as a store of its own, for `list` and `prune --plain` | `partition=plain` on `listItems`, `listItemIds`, `getItem`, `getItemContent`, `deleteItem` | None |
| `remove_plain_if_empty` | Removes `plain/` once empty | **Not needed.** The plain partition is a pointer to a generation; an empty one costs nothing, and the janitor drops it | None |

### `rewrite.rs`

| Function | What it does (FS) | Over HTTPS | `crypto/` changes |
|---|---|---|---|
| `migrate` | Publishes the journal (the lock), moves `items/` into `.rewrite/source/`, writes the stop file, runs the engine | `beginRewrite(kind: migrate)`, then the engine against the session, then `commitRewrite` | None |
| `rotate` | The same from `v2/items/`, keeping the old header in the journal | `beginRewrite(kind: rotate)`, engine, `commitRewrite`. The old header needs no keeping: until the commit it is simply the workspace's header | None |
| `run` (private; the engine) | Source store and target store over the same filesystem; `id_for`, `exists`, `import`, then `verify` each, `install`, `cleanup` | Source: the workspace, read under the old key. Target: uploads with `inRewrite`. `exists` becomes "is the id in `getRewrite`'s `stagedIds`", or simply `beginUpload`'s `created: false`. `verify` reads the staged item back... see "One gap" below | None |
| `finish` | Claims the recovery marker, reads the journal, resumes the engine or the header change | `takeOverRewrite` if another key holds it, then the engine, which skips what is staged, then `commitRewrite` | None |
| `undo` | Claims the recovery marker, puts the source back, restores the header, releases the lock | `takeOverRewrite` if needed, then `abortRewrite` | None |
| `read_plan` | Reads `.rewrite/plan.json` | `getRewrite`: kind, holder, lease, new key id, staged ids, source count | None |

### `header_change.rs`

| Function | What it does (FS) | Over HTTPS | `crypto/` changes |
|---|---|---|---|
| `restore_header` | Repairs a store whose header is missing or beside an `items/` folder, from headers v0.2.0 may have left in `v2/tmp/`, trying the words on each | **Not needed.** Those states come from two renames with a gap, from clients before v0.2.0 recreating `items/`, and from synced folders delivering a layout in pieces. None can occur: the header is one database field, changed in a transaction | None |

### `journal.rs`

| Function | What it does (FS) | Over HTTPS | `crypto/` changes |
|---|---|---|---|
| `read_journal` | Reads `.rewrite/plan.json` and tells a rewrite from a header change from an unreadable journal | `getRewrite`. There are no header-change journals: those changes are atomic | None |
| `recovery_in_progress` | Looks for `.rewrite/recovery/` and its age | `getRewrite`: `holder` and `leaseExpiresAt` | None |
| `release_recovery` | Removes a stale recovery marker so another recovery can start | `takeOverRewrite`, which the server grants only once the lease has ended. The ten-minute rule moves from the client's judgement of a folder's age to the server's clock | None |

### `open.rs`

| Function | What it does (FS) | Over HTTPS | `crypto/` changes |
|---|---|---|---|
| `open_store` | Loads the device's key file, then `open_with_key` | **Local**, unchanged, plus one `getWorkspace` | None |
| `open_with_key` | `classify`, then the table of refusals: rewriting, broken, key without encryption, no key, key mismatch; else `FsStore::new(..).plain_guarded()` or `FsStore::sealed(..).guarded()` | The same table from `getWorkspace`, without "broken"; else an `HttpStore` plain or sealed. The **guards** (`plain_guarded`, `guarded`: re-reading the header before and after every `put`, `delete`, `list_ids`) are **not needed**: every write carries `expectedKeyId` and the server refuses it atomically with `KEY_ID_MISMATCH` or `REWRITE_IN_PROGRESS` | None |

### `header.rs`, as exported

| Function | Over HTTPS |
|---|---|
| `read_header` | `getWorkspace` |
| `create_header`, `replace_header` | `enableEncryption` / `freshStart`, and `replaceHeader` |
| `write_stop_file` | **Not needed.** The stop file keeps clients before v0.2.0 out of an encrypted folder. No such client can reach a workspace: the API is new in v0.3.0 |

## What disappears

A large part of `encryption/` exists to make a directory tree behave like
a transaction; `journal.rs` and `header_change.rs` alone are 939 of its
4 384 lines, tests included. That part is: `journal.rs` (the lock as a
rename, journals staged for locks never taken, the recovery marker),
`header_change.rs` (the two-rename header swap and its repair), the stop
file, the leftovers, the guards of `open.rs`, and `fold`. Over HTTPS the
server is the transaction, so none of it has a counterpart. It all stays in
the client, unchanged, for the `ssh` and `local` backends.

## One gap, found by this mapping

The client's engine **verifies** every re-encrypted item by reading it back
from the target store and comparing SHA-256 and size (`verify` in
`rewrite.rs`). A staged item is in the next generation, which no read route
reaches: `getItemContent` reads `current` or `plain`.

Options: (a) `partition=staged`, readable by the session's holder only;
(b) verify before upload, since the client sealed the bytes itself and the
server checks the size, and rely on TLS and the server's write path;
(c) both. The client's check exists to catch a storage that acknowledged a
write and lost or damaged it, which is exactly what (b) gives up. **(a) is
recommended**, and is a small addition: one more value of `partition`, with
`LEASE_HELD` for anyone but the holder. It is recorded for the user's
decision and is not in `openapi.json` yet.

## The sketch

What the client's `encrypt` command would call, in place of functions that
take `&impl RemoteFs`. Signatures only; nothing here was implemented or
added to the client.

```rust
/// What `passalong encrypt` and `init` need from a store's encryption,
/// whatever the store is. `FsEncryptionAdmin<F: RemoteFs>` wraps today's
/// functions unchanged; `HttpEncryptionAdmin` makes the API calls.
#[async_trait]
pub trait EncryptionAdmin: Send + Sync {
    /// `inspect`.
    async fn state(&self) -> Result<StoreState, StoreError>;

    /// `read_header`: the wrapped data key, for `join`.
    async fn header(&self) -> Result<Option<StoreHeader>, StoreError>;

    /// `set_up`: an empty plaintext store becomes encrypted.
    async fn enable(&self, header: &StoreHeader) -> Result<(), StoreError>;

    /// `fresh_start`: the items are set aside, unencrypted.
    async fn fresh_start(&self, header: &StoreHeader) -> Result<(), StoreError>;

    /// `change_words`: the same key, wrapped anew.
    async fn replace_header(&self, current: &KeyId, header: &StoreHeader)
        -> Result<(), StoreError>;

    /// `migrate` and `rotate`, up to the point where items move: takes the
    /// lock and returns the two stores the engine copies between. Repeated
    /// by the same device, it returns the open rewrite.
    async fn begin_rewrite(&self, plan: &RewritePlan, new: &StoreHeader, keys: RewriteKeys<'_>)
        -> Result<Box<dyn Rewrite>, StoreError>;

    /// `read_plan` and `recovery_in_progress`: what is open, who holds it,
    /// and until when.
    async fn open_rewrite(&self) -> Result<Option<RewriteStatus>, StoreError>;

    /// `release_recovery`, then `finish` or `undo` go through the
    /// [`Rewrite`] it returns. Refused while the holder's lease runs.
    async fn take_over(&self, keys: RewriteKeys<'_>) -> Result<Box<dyn Rewrite>, StoreError>;

    /// `plain_store`.
    fn plain_store(&self) -> Box<dyn Store>;
}

/// An open re-encryption. `run` in `rewrite.rs` becomes generic over it.
#[async_trait]
pub trait Rewrite: Send + Sync {
    /// The items to re-encrypt, opened under the old key, or plaintext.
    fn source(&self) -> &dyn Store;

    /// `FsStore::id_for` and `FsStore::import`, lifted to a trait: stores
    /// `meta`'s item under the new key, keeping its creation time, unless it
    /// is there already. Over HTTPS: an upload with `inRewrite`.
    async fn import(&self, meta: &ItemMeta, content: BoxRead) -> Result<ItemMeta, StoreError>;

    /// Reads a new item back, for `verify`. See "One gap".
    async fn read_back(&self, meta: &ItemMeta) -> Result<(ItemMeta, BoxRead), StoreError>;

    /// Keeps the lock; a no-op on a filesystem.
    async fn heartbeat(&self) -> Result<(), StoreError>;

    /// `install` and `cleanup`.
    async fn commit(self: Box<Self>) -> Result<(), StoreError>;

    /// `undo`.
    async fn abort(self: Box<Self>) -> Result<(), StoreError>;
}
```

## What the v0.3.0 client has to do

1. A new crate, `passalong-https`, implementing `Store` directly, as
   "Adding a backend", route 2, of the client's architecture describes, with
   `[server.https]` in the configuration module and an opener in the
   registry.
2. Move `SealedMetaFile`, `SealedMetaBody`, and the content framing out of
   `fs_store.rs` into a module both stores use. `crypto/` is untouched.
3. Introduce `EncryptionAdmin` and `Rewrite`; wrap today's functions in the
   filesystem implementation without changing them; make `run` generic.
4. Point `commands/encrypt.rs`, `init.rs`, `check.rs`, and `prune.rs` at the
   trait instead of at `RemoteFs`. `encrypt.rs` calls `inspect` ten times
   and `set_up` seven; the call sites are many and mechanical.
5. Two things a filesystem never needed: send `heartbeat` during a long
   rewrite, and treat `REWRITE_ENDED` as "begin again under a new key",
   which `migrate` and `rotate` do anyway, since they generate the key
   inside.
6. `serve`: stop for good on `KEY_EXPIRED` and `KEY_REVOKED`; wait on
   `REWRITE_IN_PROGRESS`; on `KEY_ID_MISMATCH` keep the file and ask for
   `encrypt --join`, as it does today when a rotation overtakes a send.

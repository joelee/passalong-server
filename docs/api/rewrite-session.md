# The rewrite session

How a workspace's items are re-encrypted, `passalong encrypt` migrating a
plaintext store and `passalong encrypt --rotate`, and what happens when the
client doing it dies or repeats itself. Everything here is what
`crates/passalong-server-core/tests/model.rs` runs: 210 variants over 59
requests in 9 scenarios, with five invariants checked after every request.
It was written by PLAN-00001 for IDEA-00001-R03-MAJ-01.

Changes that re-encrypt nothing need no session. `enableEncryption`,
`replaceHeader`, and `freshStart` are each one atomic call: a fresh start
makes the current generation the `plain` partition by moving one pointer,
however many items it holds. This follows the client, where set-up, a fresh
start, and a change of words are header changes, and only `migrate` and
`rotate` run its rewrite engine.

## The idea

A workspace's items live in a numbered **generation**. A rewrite stages the
**next generation** beside the current one and then switches a pointer.

1. `beginRewrite` takes a lease and opens an empty next generation. Readers
   keep reading the current generation under the current key. Ordinary
   writers get `REWRITE_IN_PROGRESS`.
2. The holder reads each item, seals it under the new key, and uploads it
   with `inRewrite: true` and `expectedKeyId` set to the new key id. The
   upload lifecycle is the ordinary one; its target is the next generation.
   The new id follows from the item's creation time and its content under
   the new key, so an item already staged is answered with
   `created: false` and a resumed run repeats nothing.
3. The holder reads every staged item back, with `partition=staged`, and
   compares its SHA-256 and size, as the client's rewrite engine does today.
   Nobody else reads there: the generation may yet be dropped, and nobody
   else could open what is in it.
4. `commitRewrite` switches generation, header, and key id in one control
   database transaction. It is refused with `REWRITE_INCOMPLETE` unless the
   next generation holds as many items as the current one.
5. The old generation is rubbish from then on. Removing it may be cut
   short; the janitor removes any generation nothing points to.

`abortRewrite` drops the next generation instead, and the workspace is what
it was before step 1.

The server is the journal. There is no `plan.json`, no `.rewrite/` folder,
and no recovery marker: `getRewrite` says what is open, who holds it, until
when, and which ids are staged.

## The lease

- The holder renews it with `heartbeatRewrite`, and with every
  `beginRewrite` it repeats. `rewrite.lease_secs` defaults to ten minutes,
  the age at which the client today offers to take over a recovery.
- Once it has ended, another key may `takeOverRewrite`, and then resume or
  abort. This is `passalong encrypt --recover` from a second device.
  Resuming needs the words of the new key; aborting needs nothing but a
  read-write API key.
- A holder whose lease has ended is still the holder until someone takes
  over. There are never two.
- After a take-over the former holder's requests are refused with
  `LEASE_HELD`: staging, `heartbeatRewrite`, `commitRewrite`, and
  `abortRewrite` alike. An upload it began cannot commit.
- The operator can end a session from the host, with `passalong-server
  rewrite abort <workspace>`: no API key and no words are needed, since an
  abort destroys only the staged copy. While the lease runs it needs
  `--force`. The session's new key id is ended like any aborted one's.
- A session nobody recovers shuts writers out for good, exactly as a stale
  `.rewrite/` lock does today. `serve` should say so when it meets
  `REWRITE_IN_PROGRESS` past `leaseExpiresAt`.

## What a crash leaves, and what recovery does

The scripted client of the `migrate` and `rotate` scenarios sends these
twelve requests. "Stop after" means it dies having received that answer, or
having sent the request and lost the answer; the state on the server is the
same.

| # | Request | State if the client stops here | Resume (`takeOverRewrite`, stage what is missing, `commitRewrite`) | Abort (`takeOverRewrite`, `abortRewrite`) |
|---|---|---|---|---|
| 0 | `beginRewrite` | Rewriting; next generation empty | Stages both items, commits | Workspace as before |
| 1 | `beginUpload` (item 1, `inRewrite`) | Rewriting; one upload ticket | Stages both items under its own tickets, commits. The dead client's ticket goes with the session | As before; the ticket goes with the session |
| 2 | `putUploadContent` | Rewriting; ticket with content | As 1 | As 1 |
| 3 | `commitUpload` | Rewriting; item 1 staged | Skips item 1, stages item 2, commits | As before; the staged item is dropped |
| 4 | `heartbeatRewrite` | As 3, lease renewed | As 3, after the renewed lease ends | As 3 |
| 5 | `beginUpload` (item 2) | Rewriting; item 1 staged, one ticket | As 3 | As 3 |
| 6 | `putUploadContent` | As 5 | As 3 | As 3 |
| 7 | `commitUpload` | Rewriting; both staged | Stages nothing, commits | As before |
| 8 | `commitRewrite` | **Sealed under the new key.** No session | Nothing to recover | Nothing to abort: `abortRewrite` answers the current state, which names the new key, so the caller sees the rewrite was committed |
| 9 | `beginUpload`, an ordinary one under the new key | Sealed; one upload ticket | Nothing to recover; the janitor forgets the ticket | — |
| 10 | `putUploadContent` | Sealed; ticket with content | As 9 | — |
| 11 | `commitUpload` | Sealed; the new item stored | Nothing to recover | — |

After every recovery the dead client's remaining requests are delivered
anyway, as a zombie's would be. Each is answered or refused and none breaks
an invariant:

- after a resume, its staging requests get `NOT_FOUND` (no session), and
  its `commitRewrite` is answered with the state already reached;
- after an abort, its staging requests get `NOT_FOUND`, and its
  `commitRewrite` gets `NOT_FOUND`, because the workspace is not under the
  key it names.

The same runs with a shelf whose first removal of a generation is cut
short, standing for a crash between `commitRewrite`'s transaction and its
clean-up. The janitor finishes the job.

### The invariants

| | Checked after every request |
|---|---|
| I1 | The workspace is in exactly one state; a session exists exactly when it is `REWRITING`; a plaintext workspace has no key and a sealed one has a key and a header |
| I2 | Every item of the current generation was uploaded under the workspace's key; every staged item under the session's new key; every plain item under none |
| I3 | Nothing is lost and nothing invented: every text stored before is stored, and nothing is stored that was not sent |
| I4 | The bytes the control database counts equal the bytes on the shelf, and every staging place belongs to an upload |
| I5 | An upload id has one outcome, and every replay returns it |

Once everything has settled and the janitor has passed: no session, no
staging place, no upload, no reservation, and no generation that nothing
points to.

## Replays

| Operation, sent again | Answer |
|---|---|
| `beginUpload`, same key, same request, ticket live | The same ticket; nothing more is reserved |
| `beginUpload` for content already stored | `{ item, created: false }` |
| `putUploadContent` before the commit | Replaces what was sent |
| `putUploadContent` after the commit | 204; the content is discarded |
| `commitUpload` within the retention window | The same outcome, with the original `created` |
| `commitUpload` after it | `NOT_FOUND`; settle with `getItem` |
| `abortUpload` | 204, always |
| `deleteItem` of an item already gone | `NOT_FOUND`, which means done |
| `cleanStaging`, `probeWrite` | Safe to repeat by nature |
| `enableEncryption`, `freshStart`, same key id | The state already produced; nothing else is set aside |
| `enableEncryption`, `freshStart`, another key id | `KEY_ID_MISMATCH` |
| `replaceHeader` | Writes the same header again |
| `beginRewrite` by the holder, same kind and new key id | The live session, lease renewed |
| `beginRewrite` after that rewrite was **committed** | `KEY_ID_MISMATCH`: the key it expects is no longer current |
| `beginRewrite` after that rewrite was **aborted** | `REWRITE_ENDED` |
| `heartbeatRewrite`, `takeOverRewrite` by the holder | The session, lease renewed |
| `commitRewrite` after it succeeded, naming the new key id | The state it produced. The replay that matters most: the client learns that the commit happened |
| `abortRewrite` with no session open | The current state |

### `REWRITE_ENDED`

The model test found this. A duplicate of `beginRewrite` that arrives after
its rewrite was aborted is, to the server, a new request: the workspace is
exactly as it was. Accepted, it would open a session nobody holds and shut
every writer out until someone ran `encrypt --recover`. The client today is
not exposed to this, because its lock is taken by a local rename, which
nothing can replay.

So the server remembers the new key id of every aborted rewrite, for good,
and refuses a `beginRewrite` that names one. This costs a client nothing:
it generates a fresh data key for every attempt, so a real new attempt
never names an old key id. A client that gets `REWRITE_ENDED` begins again
under a new key.

## What the server still cannot check

- That a staged item is the re-encryption of a source item. It compares
  counts, as it must: the ids differ, and it cannot open either. The client
  can, and does: it reads every new item back with `partition=staged` and
  compares SHA-256 and size before it commits. The model test's resume path
  does the same.
- That the new header wraps the key the items are sealed under. A client
  that commits the wrong header locks its own workspace; the old generation
  is gone by then. The client should unwrap the header it is about to send
  with the words, once, before `beginRewrite`.

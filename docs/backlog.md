# Backlog

Future work not covered by an active plan. Completed items are removed.

## @joelee road map for next releases

### v0.1.0
- **Decide IDEA-00001**: answer its blocking questions, run the protocol
  spike of its §14, then accept, revise, or park it.
- **PLAN-00001**, once the idea is accepted: the server with parity for
  passalong client v0.2.1, its Docker image, and its systemd unit.
- **IDEA-00001 r04**: record the protocol spike's verdict (PLAN-00001) and
  the user's decision to accept, revise, or park the idea.
- **Read-back of staged items** (`partition=staged` for the session's
  holder), so the client can verify a re-encrypted item before it commits,
  as it does today; see `docs/api/client-encryption-mapping.md`, "One gap".
  A decision for the user; not in `openapi.json` yet.

### Unscheduled
- An open-source licence. Until then nothing is published.
- **Publishing**, once a licence exists: image and binaries, the release
  workflow's publish job, and `THIRD-PARTY-NOTICES` in every artefact
  (reopens IDEA-00001-R02-MED-03).

## Agent suggested next steps

### Features

- **`workspace import` and `export`** for stores from the client's `ssh` and
  `local` backends; the on-disk files are byte-identical by design.
- **`GET /v1/events`**, a server-sent-events stream, so pull mode need not
  poll.
- **A way for systemd hosts to get the binary** under a build-only
  release: a `just install` recipe, or documented `cargo build` steps.
- **Server-side retention** per workspace by age and count, which works for
  encrypted workspaces too because ids carry creation time.
- **`passalong-server backup`**: a consistent copy while the server runs.
- **A send-only role**, for devices that contribute but must not read.
- **Remote administration** with an admin-scoped key.
- **A static musl build and a distroless image.**
- **Metrics endpoint.**
- **ACME.**

### Process

- **`just test-integration`, `test-client`, `test-deploy`, and `openapi`**
  recipes, named in the `justfile`, arrive with the plans that need them.
- **macOS job in CI** is deliberately absent: the server supports Linux
  only. Revisit if developers need to build on macOS.

# Backlog

Future work not covered by an active plan. Completed items are removed.

## @joelee road map for next releases

### v0.1.0
- **The next slices of server v0.1.0**, each with its own plan (PLAN-00002
  D-01), now that the storage slice is done (PLAN-00002, completed):
  1. Done: API keys and the operations CLI (PLAN-00003).
  2. Done: the HTTP surface and TLS (PLAN-00004). `serve`, every operation
     of the contract as a route, authentication, limits, rate limiting, the
     plaintext content check, the janitor, TLS with pins.
  3. **Next.** Docker and systemd: `service install`, the image,
     `deploy/docker`. It inherits: `serve` stops on SIGTERM within 30
     seconds, so a unit's `TimeoutStopSec` and a container's stop timeout
     must be longer; `check --health` is the health check and needs to read
     the configuration and, in `tls` mode, the certificate; `init` expects
     the pair in `tls/` beside the configuration file.
- **The client's v0.3.0 backend plan**, in the client repository, from
  `docs/api/openapi.json` and `docs/api/client-encryption-mapping.md`, once
  that first slice confirms the contract needs no change.

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

- **An in-memory id index per workspace.** Listing reads the directory;
  measure before optimising.

- **Rate limiting of failed authentications**: `limits.auth_failures_per_minute`
  is parsed and waits for the HTTP slice, which has the client address.
- **`passalong-server audit`**: the audit trail is written since PLAN-00003
  and has no command to read it yet.
- **A send-only role** stays deferred (IDEA-00001 r04 §15).

### Process

- **`just test-integration`, `test-client`, `test-deploy`, and `openapi`**
  recipes, named in the `justfile`, arrive with the plans that need them.
- **macOS job in CI** is deliberately absent: the server supports Linux
  only. Revisit if developers need to build on macOS.

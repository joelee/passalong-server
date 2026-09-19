# Backlog

Future work not covered by an active plan. Completed items are removed.

## @joelee road map for next releases

### v0.1.0
- **Server v0.1.0 is built**, in four slices after the protocol spike:
  storage (PLAN-00002), API keys and the operations CLI (PLAN-00003), the
  HTTP surface and TLS (PLAN-00004), Docker and systemd (PLAN-00005). What
  is left is the user's: whether and when to tag `v0.1.0`. A tag verifies
  the build, for amd64 and arm64, and publishes nothing.
- **Next: the client's v0.3.0 backend plan**, in the client repository, from
  `docs/api/openapi.json` and `docs/api/client-encryption-mapping.md`. The
  contract needed no change while the server was built (PLAN-00004,
  REQ-11). Until a client exists, the server has been driven only by its own
  small client and by `curl`.
- **arm64** has never been built: the release workflow does it on a tag, and
  no tag has been pushed. Expect the first one to find something.

### Unscheduled
- **Active, PLAN-00006.** Before v0.1.0 is tagged: AGPL v3, third-party
  notices, publishing to Docker Hub and GitHub on a tag, the source URL
  wherever the program speaks, `tls letsencrypt`, `audit`.
- **crates.io**: every manifest keeps `publish = false`. Nobody has asked.
- **Signing and provenance** for the image and the archives.
- **A contributor agreement**, if the freedom to relicense alone matters
  once others contribute under AGPL.

## Agent suggested next steps

### Features

- **`workspace import` and `export`** for stores from the client's `ssh` and
  `local` backends; the on-disk files are byte-identical by design.
- **`GET /v1/events`**, a server-sent-events stream, so pull mode need not
  poll.
- **Server-side retention** per workspace by age and count, which works for
  encrypted workspaces too because ids carry creation time.
- **`passalong-server backup`**: a consistent copy while the server runs.
- **A send-only role**, for devices that contribute but must not read.
- **Remote administration** with an admin-scoped key.
- **A static musl build and a distroless image.**
- **Metrics endpoint.**
- **ACME** in the server. `tls letsencrypt` prints how to do it with
  `certbot` meanwhile.

- **An in-memory id index per workspace.** Listing reads the directory;
  measure before optimising.


### Process

- **`just test-client`**: a released passalong client against this server,
  once there is one.
- **macOS job in CI** is deliberately absent: the server supports Linux
  only. Revisit if developers need to build on macOS.

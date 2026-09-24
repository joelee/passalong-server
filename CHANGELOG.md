# Changelog

All notable changes to passalong-server. Versions follow
[SemVer](https://semver.org/); the server's version is independent of the
passalong client's.

## Unreleased

- The release workflow fails at once, before building, when a tag run
  finds `DOCKERHUB_USERNAME` or `DOCKERHUB_TOKEN` unset.
## v0.1.0 - 2026-09-22T16:27:46Z

The first release. Everything below is new.

- `key create` and `docs/usage.md` no longer say the key goes into `.env`
  on the device: the passalong client keeps it in an owner-only file.
  Raised by the client's v0.3.0 build.
- Contract: a rewrite session carries `newHeader`, the header `beginRewrite`
  brought. Without it, a device that took a dead session over had the new
  words and nothing for them to unlock, and could only abort; nor could the
  holder resume after a restart. Found by the client's v0.3.0 plan
  (PLAN-00010 D-05) before any client was written. Additive.
- **A release tag publishes**: `joeworks/passalong-server` on Docker Hub for
  amd64 and arm64 (`X.Y.Z`, `X.Y`, `latest`), and a GitHub release with an
  archive per architecture and `SHA256SUMS`. Run by hand, the workflow does
  everything but publish. CI runs on every branch and pull request.
  `scripts/release-archive.sh` makes and verifies the archives, and `just
  ci` tries it on the developer's machine.
- `passalong-server tls letsencrypt --host <name>`: prints, for this host's
  configuration, the `certbot` command and a deploy hook that installs every
  renewed pair for the server's user, with `--docker` for the compose
  project. It changes nothing. The command includes `--reuse-key`, because a
  renewal with a new key would lock out every device that pinned the old.
- `passalong-server audit`: reads the audit trail of workspace and key
  changes, which has been written since PLAN-00003 and had no reader.
- Contract: `getViewer` answers `server.sourceUrl`, where the server's source
  is. Additive and optional for a client; the first change to the contract
  since it was written.
- The source is one line away: every help screen ends with the repository's
  address and the licence; the image carries OCI labels for source, licence,
  version, and revision; the README opens with the link.
- `THIRD-PARTY-NOTICES`: the licence of every crate the binary links, made
  by `just notices` from `Cargo.lock`, kept current by `just ci`, and carried
  by the image beside `LICENSE`.
- **The licence is the GNU Affero General Public License, version 3 or
  later** (`AGPL-3.0-or-later`), in place of the proprietary notice under
  which nothing could be distributed (PLAN-00006). Dependencies stay
  permissive-only.
- The kill harness kills its child with SIGKILL instead of aborting it. An
  abort dumps core, and a run of the harness did so hundreds of times: each
  a crash report on a desktop that announces them, and, where the kernel
  writes cores beside the process, a file in the repository. The harness
  now fails if its child dies by any other signal.
- Docker and systemd (PLAN-00005). `passalong-server service install`, as
  root, sets a systemd host up: binary, user, directories, configuration,
  if asked a self-signed pair, and the hardened unit, overwriting nothing;
  `service remove` takes the unit away and leaves everything else.
  `deploy/docker/` now uses two named volumes, for data and for
  configuration with the TLS pair, and has a README that a test follows.
  `init` writes to `PASSALONG_SERVER_CONFIG_FILE` when that is set. Both
  installations are tested end to end (`just test-deploy`, `just
  test-service`), and `just ci` runs them. Fixed: **the image did not
  build** since PLAN-00003, because the `Dockerfile` did not copy
  `config.sample.toml`, which the binary embeds; the compose draft's host
  folders were created by Docker as root, where the server could not write
  and every command refused to run; and `service install` on a host without
  `/etc/sysusers.d`.
- The HTTP surface and TLS (PLAN-00004): `passalong-server serve`. Every
  operation of the contract is a route, behind authentication, limits, and
  TLS; `tls self-signed`, `tls fingerprint`, `check --health`. Failed
  authentications are rate-limited per address (`RATE_LIMITED`). The
  certificate pair is re-read when it changes on disk. `init` now expects
  the pair in `tls/` beside the configuration file. The control database
  moves to schema version 3, migrated in place at opening: copy
  `control.sqlite` first. Found and fixed on the way: `meta` and the
  encryption header came back re-spelled instead of byte for byte; a slow
  upload held up every other request of its workspace; leases and
  `receivedAt` went by the machine's clock and not the injected one; and
  the commit of an upload staged for a rewrite answered the workspace's
  item of the same id.
- API keys and the operations CLI (PLAN-00003): keys that expire, can be
  revoked, and are stored only as hashes; workspaces; the configuration
  file; logging; `passalong-server init`, `workspace`, `key`, `rewrite`,
  and `check`. The control database moves to schema version 2, migrated in
  place at opening: copy `control.sqlite` first, since a version-2 database
  does not open in an older build.
- Storage under the rules (PLAN-00002): a filesystem shelf and a SQLite
  control database behind the protocol spike's rules, reconciliation at
  start-up, and a harness that kills the process at every boundary between
  the two. Found and fixed on the way: a destructive shelf step taken before
  its record was stored (a rewrite's commit, and the janitor); an upload id
  handed out twice by a random source that repeats itself; several
  processes opening a new database at once being told it was locked; and a
  ticket left behind when its content turned out to be stored.
- Protocol spike (PLAN-00001): the workspace, upload, and rewrite-session
  rules as an in-memory model in `passalong-server-core`, with a model test
  that interrupts and replays a client at every step; `docs/api/openapi.json`;
  the mapping of the client's encryption code onto the API. Found and fixed
  by the model test: a late duplicate of `beginRewrite` reopening an aborted
  rewrite (`REWRITE_ENDED`). `limits.max_item_bytes` defaults to
  `"unlimited"`; a fresh start is one atomic call.
- Contract: `putUploadContent` can answer `CONTENT_MISMATCH`, since content
  longer than announced is refused as it arrives.
- IDEA-00001 accepted (r04), after the protocol spike.
- `partition=staged` on the read routes, for a rewrite session's holder
  alone, so the client can verify every re-encrypted item before it commits.
- Project scaffold: workspace, process documents, and the first idea report
  (IDEA-00001). No server functionality yet.
- IDEA-00001 r02, after the review of r01, and the draft documents brought
  in line with it: upload replay semantics, a control database that fails
  closed, and an item-size limit that is the server's alone.
- IDEA-00001 r03 and the drafts brought in line with four decisions: the
  API is REST + JSON, not GraphQL; the server never exposes a field taken
  from an item's `meta`; server v0.1 carries the full rewrite session; and
  nothing is published, so the release workflow only verifies the build.

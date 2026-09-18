# Changelog

All notable changes to passalong-server. Versions follow
[SemVer](https://semver.org/); the server's version is independent of the
passalong client's.

## Unreleased

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

# Changelog

All notable changes to passalong-server. Versions follow
[SemVer](https://semver.org/); the server's version is independent of the
passalong client's.

## Unreleased

- Project scaffold: workspace, process documents, and the first idea report
  (IDEA-00001). No server functionality yet.
- IDEA-00001 r02, after the review of r01, and the draft documents brought
  in line with it: upload replay semantics, a control database that fails
  closed, and an item-size limit that is the server's alone.
- IDEA-00001 r03 and the drafts brought in line with four decisions: the
  API is REST + JSON, not GraphQL; the server never exposes a field taken
  from an item's `meta`; server v0.1 carries the full rewrite session; and
  nothing is published, so the release workflow only verifies the build.

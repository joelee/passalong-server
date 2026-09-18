# Developer guide

## Set-up

`rust-toolchain.toml` pins the toolchain, the same one as the passalong
client. Then:

```text
just setup     # cargo-llvm-cov, cargo-deny, actionlint
pre-commit install
just check     # format, lint, links, tests, coverage, build
```

`just ci` adds the dependency audit, the workflow lint, and the image build.
Nothing is ever pushed: the image stays local.

## Rules

[AGENTS.md](../AGENTS.md) holds them: test first, mock external interfaces,
line coverage of at least 80 %, no secrets anywhere, documents updated with
the change.

## Layout

| Path | What lives there |
|---|---|
| `crates/passalong-server-core/` | The domain; no HTTP, no terminal |
| `crates/passalong-server-api/` | The HTTP surface; no domain rules |
| `crates/passalong-server-cli/` | The `passalong-server` binary |
| `docs/api/` | The contract the client implements |
| `docs/ideas/`, `docs/plans/`, `docs/reviews/` | Numbered reports; each folder's `AGENTS.md` is its contract |
| `docs/release/` | One file of release notes per version |
| `docs/service/` | The systemd unit, kept identical to the rendered template by a test |
| `deploy/docker/` | Compose file for running a server |
| `scripts/` | Release and link checks, shared with the client |

## The client

The client is a separate, Apache-2.0 repository, expected beside this one
at `../passalong/`. Nothing from this repository may be copied into it, and
it never depends on a crate from here. Integration tests that need a client
use a released binary, fetched and checksum-verified as the client's
`test-compat` recipe does.

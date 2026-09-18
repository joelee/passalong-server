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

## The tests that matter most

All run in `just check`, in a few seconds.

| Test | What it shows | How to read it |
|---|---|---|
| `tests/model.rs` | The rules survive a client that stops, repeats, or returns as a zombie at every request: 210 variants, over memory and over the filesystem with SQLite | `cargo test -p passalong-server-core --test model -- --nocapture` prints the variant counts. A failure names the scenario, the request, and the invariant (I1 to I5) |
| `tests/kill.rs` | The stores survive the *process* being killed at every passage of every fault point | `… --all-features --test kill -- --nocapture` prints how often each point fired. It fails if one never fired, and its control test fails if the repair is ever not needed |
| `tests/crash_in_process.rs` | The same states, provoked without processes, where they are quick to debug | Each test names the flaw it was written for |
| `tests/two_processes.rs` | Several processes share a workspace, as the CLI and the server will | A child's warnings and errors are in the failure message |
| `tests/shelf_conformance.rs`, the ledger suite in `src/ledger/mod.rs` | Every shelf and every ledger behaves the same | One suite, instantiated per implementation |
| `tests/logs.rs` | Nothing of an item reaches the logs | |
| `tests/openapi.rs` | `docs/api/openapi.json` and `docs/api/README.md` describe one API | |

The kill harness and the two-process test need the `fault-injection`
feature, which `just` recipes enable with `--all-features`. It compiles
named abort points and the `storage_child` binary; the server's build has
neither (`src/fault.rs`). To add a fault point: name it in `fault::POINTS`,
call `fault::point` where the code crosses from one store to the other, and
the harness will insist that it fires.

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

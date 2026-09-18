# Usage

`passalong-server` is one binary: the operator's commands today, and the
daemon with the HTTP slice of v0.1.0. Every command line on this page is run
by `crates/passalong-server-cli/tests/session.rs`, except those marked as
belonging to a later slice.

## From an empty host

```text
passalong-server init
passalong-server workspace create home --quota 20GiB
passalong-server key create --workspace home --label laptop --expires 90d
passalong-server key list
passalong-server check
```

`init` writes a configuration file and creates the data directory, mode 700.
As root that is `/etc/passalong-server/config.toml` and
`/var/lib/passalong-server`; as anyone else, below `~/.config` and
`~/.local/share`. `--config-file` and `--data-dir` choose otherwise. It never
overwrites a configuration that is there.

Every other command finds the configuration as
[configuration](configuration.md) describes, or takes `--config <file>`.
Every listing takes `--json`.

## Workspaces

A workspace is one passalong store: the devices that share a clipboard.

| Command | What it does |
|---|---|
| `passalong-server workspace create <name> [--quota 20GiB]` | Names are lower-case letters, digits, and dashes, at most 32, a letter first. Without `--quota` the workspace follows `limits.workspace_quota_bytes` |
| `passalong-server workspace list` | Name, quota, creation, id |
| `passalong-server workspace show <name>` | Items, bytes used and promised to uploads, quota, encryption state and key id, an open rewrite session, and its keys. Opening a workspace also repairs what an unclean stop left, so this is the operator's way to do that |
| `passalong-server workspace delete <name> [--yes] [--force]` | Deletes the workspace, every item, and every key. It asks for the name to be typed again, or takes `--yes`. While a rewrite session is open it needs `--force` |

## Keys

A key belongs to exactly one workspace, and opens that workspace and no
other. On the device it goes into `.env` as `PASSALONG_API_KEY`.

| Command | What it does |
|---|---|
| `passalong-server key create --workspace <name> --label <text> [--expires 90d \| --never] [--read-only]` | Prints the key **once**. Without `--expires` it lasts 90 days and says so; a key that never expires has to be asked for with `--never` |
| `passalong-server key list [--workspace <name>]` | Id, workspace, label, role, state (`active`, `expired`, `revoked`), expiry, last use to the minute. Never a secret, nor a hash |
| `passalong-server key extend <id> (--expires 30d \| --never)` | A new expiry, counted from now, for the same secret: nothing to redistribute. It does not bring a revoked key back |
| `passalong-server key revoke <id>` | The key is refused from the next request on, by every process: keys are read from the database on each request. The row stays, for the audit trail |
| `passalong-server key delete <id>` | Removes the key's record |
| `passalong-server key prune --older-than 30d` | Deletes keys that expired or were revoked that long ago |

**A lost key cannot be shown again.** Only the SHA-256 of its secret is
kept, so there is nothing to show. Create another and revoke the lost one.

Durations are a whole number and `s`, `m`, `h`, `d`, or `w`.

## A rewrite session nobody will finish

`passalong encrypt` migrating or rotating a workspace holds a session on the
server, and while it is open every writer is refused. If the device doing it
dies, another device can finish or abort it with `passalong encrypt
--recover`. If none will, the operator can:

| Command | What it does |
|---|---|
| `passalong-server rewrite show <workspace>` | Kind, the key that holds it, how much is staged, and whether its lease has ended |
| `passalong-server rewrite abort <workspace> [--force]` | The workspace is again what it was before; what was staged is dropped. It needs no API key and no words, because aborting destroys nothing but the staged copy. While the holder's lease runs, the device may still be at work, and this needs `--force` |

## Check

`passalong-server check` prints one line per step: the configuration, the
data directory and its mode, the control database and its schema version,
and every workspace, which it opens and so repairs. It exits 1 if a step
failed. A directory under `workspaces/` that belongs to no workspace is
reported: a deletion was cut short there.

## Who may run the commands

Every command but `init` refuses to run as any user but the data
directory's owner, root included:

```text
error: /var/lib/passalong-server belongs to user 10001, and this is user 0. …
Run it as the owner:
  sudo -u '#10001' passalong-server key list
```

The server and the commands share one database. A database or journal file
created by root would lock the server out, and a server that cannot read its
keys refuses every request rather than guess (see
[Failing closed](architecture.md#failing-closed)). The check reads
`/proc/self`; where there is no `/proc`, it fails closed and says so.

## With Docker (the Docker slice)

Build the image first, since none is published (`docker compose build` in
`deploy/docker/`), then run the commands inside the container, which runs as
the data directory's owner, so they need no `--user`:

```text
docker compose exec server passalong-server key create --workspace home --label laptop
```

## Not in this build yet

| Command | Arrives with |
|---|---|
| `passalong-server serve` | The HTTP slice |
| `passalong-server tls self-signed \| fingerprint` | The HTTP slice |
| `passalong-server check --health` | The HTTP slice |
| `passalong-server service install \| remove` | The Docker and systemd slice |

They exist and say so.

## Exit codes

`0` done; `1` refused or failed, with the reason on standard error; `2` a
command line that does not parse. What a command prints for the operator
goes to standard output; logs go to standard error.

## On a device

passalong v0.3.0 and later: `passalong init`, choose `https`, and give the
URL and the key. See the client's documentation.

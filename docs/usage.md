# Usage

`passalong-server` is one binary: the server, and the operator's commands.
Every command line on this page is run by the tests in
`crates/passalong-server-cli/tests/`, except those marked as belonging to a
later slice.

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

## TLS

The server speaks TLS itself (`listen.mode = "tls"`, the default) or plain
HTTP behind a proxy that does (`"plain"`). In `tls` mode it needs a
certificate and its key, at `tls.cert_file` and `tls.key_file`; `init` sets
those to `tls/` beside the configuration file. Use a pair from your
certificate authority, or make one:

```text
passalong-server tls self-signed --host nas.example --ip 192.0.2.4
passalong-server tls fingerprint
```

`--host` and `--ip` are what clients will type, each as often as needed.
The key is written for its owner alone, and nothing is ever written over a
pair that is there: clients may have pinned it. Nobody signed this
certificate, so clients connect by its **pin**, which both commands print:
`sha256/` and the base64 of the SHA-256 of the certificate's public key
info. It is the form of the client's `tls_pin` and, with two slashes, of
`curl --pinnedpubkey`. The pin names the key, not the certificate: a
renewal that keeps the key keeps the pin.

The pair is read again within half a minute of changing on disk, so
certbot and its kind need no hook and no restart. A pair that does not load
is logged, and the one before it stays in use.

There is no switch that turns certificate checking off, on either side.

## Running the server

```text
passalong-server serve
passalong-server check --health
```

`serve` logs what it listens on and serves until SIGTERM or Ctrl-C. Then it
accepts nothing new, lets the requests in flight finish, for at most half a
minute, and exits 0. In `tls` mode without its pair it does not start, and
says which files it looked for. It never falls back to plain HTTP.

Workspaces and keys made, changed, or revoked with the commands above hold
from the next request on; the server needs no restart and no signal.

Every ten minutes the server removes uploads that were begun and not
finished within `staging.max_age_hours`.

`check --health` asks the running server's `/readyz` and exits 0 if it is
ready and 1 if not; it is what the `Dockerfile`'s `HEALTHCHECK` runs. In
`tls` mode it connects by the pin of the configured certificate. `/healthz`
answers 204 while the process runs; `/readyz` answers 204 only while the
control database and the data directory can be used, and 503 otherwise, when
the server is [failing closed](architecture.md#failing-closed).

## A session with curl

The passalong client is the intended client, from its v0.3.0. Until then,
and for looking at a server, `curl` will do. This is a real session against
a server with a self-signed pair; `curl ...` stands for

```text
curl -sS --cacert tls/cert.pem --pinnedpubkey "sha256//<the pin, after sha256/>" https://localhost:8443
```

and `$KEY` for the key `key create` printed. An item's id is eight hex
digits of time, a dash, and the first twelve of its content's SHA-256; a
plaintext workspace checks the content against `meta` at the commit.

```text
$ curl ... /healthz -o /dev/null -w "%{http_code}\n"
204
$ curl ... /v1/viewer   # no key
{"code":"UNAUTHENTICATED","retryable":false,"status":401,"title":"no valid API key"}
$ curl ... -H "Authorization: Bearer $KEY" /v1/viewer
{"key":{"expiresAt":"2026-12-17T22:40:40Z","id":"a4a8da952b57","label":"laptop","role":"readWrite"},"server":{"apiVersion":1,"maxItemBytes":null,"version":"0.1.0"}}
$ curl ... -X POST /v1/uploads -d '{"id":"6b49d200-2cf24dba5fb0","meta":{...},"size":"5"}'
{"uploadId":"3b65ad156dd7f1b0fd7303ce6caee434","expiresAt":"2026-09-19T22:40:57Z"}
$ curl ... -X PUT --data-binary @hello.txt /v1/uploads/$UPLOAD/content -w "%{http_code}\n"
204
$ curl ... -X POST /v1/uploads/$UPLOAD/commit
{"item":{"id":"6b49d200-2cf24dba5fb0","meta":{"schema":1,"kind":"text","sha256":"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824","size":5},"storedBytes":"5","receivedAt":"2026-09-18T22:40:57Z"},"created":true}
$ curl ... /v1/items
[{"id":"6b49d200-2cf24dba5fb0","meta":{"schema":1,"kind":"text","sha256":"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824","size":5},"storedBytes":"5","receivedAt":"2026-09-18T22:40:57Z"}]
$ curl ... -H "Range: bytes=1-3" /v1/items/$ID/content
ell
$ curl ... /v1/workspace
{"name":"home","quotaBytes":"21474836480","usedBytes":"5","itemCount":1,"encryption":{"state":"plaintext","keyId":null,"header":null,"rewrite":null}}
$ curl ... --pinnedpubkey sha256//AAAA...= /healthz   # another pin
curl: (90) SSL: public key does not match pinned public key
```

## Not in this build yet

| Command | Arrives with |
|---|---|
| `passalong-server service install \| remove` | The Docker and systemd slice |

It exists and says so.

## Exit codes

`0` done; `1` refused or failed, with the reason on standard error; `2` a
command line that does not parse. What a command prints for the operator
goes to standard output; logs go to standard error.

## On a device

passalong v0.3.0 and later: `passalong init`, choose `https`, and give the
URL and the key. See the client's documentation.

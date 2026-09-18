# Configuration

> Read by `passalong-server` since PLAN-00003. The `listen` and `tls`
> sections are parsed and validated today and used by `serve`, which arrives
> with the HTTP slice. [`config.sample.toml`](../config.sample.toml) is the
> file `init` writes, and a test keeps it valid.

## Where the file is found

The first of these that exists:

1. the `--config` argument
2. `PASSALONG_SERVER_CONFIG_FILE`
3. `$XDG_CONFIG_HOME/passalong-server/config.toml`
4. `$HOME/.config/passalong-server/config.toml`
5. `/etc/passalong-server/config.toml`
6. `./config.toml`

Unknown keys and sections are errors, naming the key and the line, as in
the client. A file that `--config` or `PASSALONG_SERVER_CONFIG_FILE` names
must exist. Sizes are a number of bytes, alone or with `KiB`, `MiB`, `GiB`,
or `TiB` (`"20 GiB"`); `"unlimited"` where the table says so.

## Keys

| Key | Default | Meaning |
|---|---|---|
| `server.log_level` | `info` | `error`, `warning`, `info`, `verbose`, or `debug`. Logs go to standard error, and only an allow-list of fields is ever written: ids, counts, outcomes, and what a store said |
| `server.data_dir` | `/var/lib/passalong-server` | Workspaces, the control database, staging. On a local filesystem: the control database uses SQLite's WAL mode, which needs shared memory that network filesystems do not give |
| `listen.address` | `0.0.0.0:8443` | Address and port |
| `listen.mode` | `tls` | `tls`, or `plain` behind a TLS-terminating proxy. `plain` on an address that is not loopback is refused unless `listen.behind_proxy` is true |
| `listen.behind_proxy` | `false` | Required for `plain` on a non-loopback address; also makes the server trust `X-Forwarded-For` for rate limiting |
| `tls.cert_file`, `tls.key_file` | none; `init` writes `tls/cert.pem` and `tls/key.pem` beside the configuration file | PEM files, looked at every 30 seconds and re-read when they changed. `serve` in `tls` mode does not start without them; [`tls self-signed`](usage.md#tls) makes a pair |
| `limits.max_item_bytes` | `"unlimited"` | Largest item; see [below](#the-item-size-limit). Clients read the value before they upload |
| `limits.workspace_quota_bytes` | `20 GiB` | Quota of a new workspace. While a rewrite is open its next generation has an allowance of the same size, so plan disk for twice the quota of a workspace that is being re-encrypted |
| `limits.auth_failures_per_minute` | `10` | Per client address, then `RATE_LIMITED`; `0` turns it off. See [below](#failed-authentications) |
| `rewrite.lease_secs` | `600` | How long a rewrite session stays its holder's without a heartbeat, before another device may take it over. The client today waits the same ten minutes before it offers to take over a recovery |
| `staging.max_age_hours` | `24` | Age at which the janitor removes unfinished uploads. Also the least time a committed upload's outcome is kept, so that a repeated `commitUpload` gets the same answer |

## Failed authentications

A request with no key, or with one that is not a key of this server, is a
guess, and guesses are counted per client address over a sliding minute.
At `limits.auth_failures_per_minute` the address is answered
`RATE_LIMITED` (429) with a `Retry-After`, whatever it sends, a right key
included, until its oldest guess is a minute old. Requests that succeed are
never counted. Nor is an expired or revoked key: that was the right secret,
and a forgotten device that keeps sending one must not shut out everyone
behind the same router. `/healthz` and `/readyz` are not limited.

The address is the connection's. With `listen.behind_proxy = true` it is
the **last** entry of `X-Forwarded-For`, the one your proxy appended;
entries before it came from the client and are ignored, so a forged header
moves nothing. This is right for exactly one proxy in front of the server.
Behind two, every client appears as the outer proxy's address and shares
one count; raise the limit or turn it off, and limit at the outer proxy.
If the proxy sends no such header, all clients count as the proxy.

An IPv6 client is counted as its /64. The server remembers at most 10,000
addresses; when there are more, the one that failed longest ago is
forgotten first.

## The item-size limit

`limits.max_item_bytes` defaults to `"unlimited"`, decided by the protocol
spike (PLAN-00001, REQ-07):

- **Parity.** The client's sizes are 64-bit and its `ssh` and `local`
  backends set no limit. A default limit would make the same `passalong
  file` succeed over SSH and fail here.
- **The quota is the bound that matters.** `beginUpload` refuses an item
  that does not fit the workspace's quota together with what is stored and
  what is promised to other uploads, before a byte is sent, and the stream
  is cut off at the announced size. A limit per item protects nothing the
  quota does not: whoever may upload one large item may upload many small
  ones.
- **Exposure.** Only a valid API key gets as far as `beginUpload`; the key
  is checked before any body is read, and failed authentications are
  rate-limited. A server on the internet is no more exposed by large items
  than by many.
- **Memory** is not at stake: content streams to disk.

Set a size where one item should not be able to take a whole workspace,
for example a shared workspace with a small quota. The client reads the
limit from `getViewer` and refuses the file itself, naming the limit,
rather than learning of it from `ITEM_TOO_LARGE`.

## Environment

| Variable | Meaning |
|---|---|
| `PASSALONG_SERVER_CONFIG_FILE` | Path to `config.toml` |
| `PASSALONG_SERVER_LOG_LEVEL` | Overrides `server.log_level` |

The server needs no secret in its environment. API keys live hashed in the
control database; the TLS private key is a file.

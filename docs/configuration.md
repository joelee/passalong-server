# Configuration

> **Draft.** No code reads this configuration yet; the keys are the proposal
> of [IDEA-00001](ideas/00001-HTTPS_Server_Backend-r01.md).
> [`config.sample.toml`](../config.sample.toml) shows them with defaults.

## Where the file is found

The first of these that exists:

1. the `--config` argument
2. `PASSALONG_SERVER_CONFIG_FILE`
3. `$XDG_CONFIG_HOME/passalong-server/config.toml`
4. `$HOME/.config/passalong-server/config.toml`
5. `/etc/passalong-server/config.toml`
6. `./config.toml`

Unknown keys are rejected, as in the client.

## Keys

| Key | Default | Meaning |
|---|---|---|
| `server.log_level` | `info` | `error`, `warning`, `info`, `verbose`, or `debug` |
| `server.data_dir` | `/var/lib/passalong-server` | Workspaces, the control database, staging |
| `listen.address` | `0.0.0.0:8443` | Address and port |
| `listen.mode` | `tls` | `tls`, or `plain` behind a TLS-terminating proxy |
| `listen.behind_proxy` | `false` | Required for `plain` on a non-loopback address; also makes the server trust `X-Forwarded-For` for rate limiting |
| `tls.cert_file`, `tls.key_file` | none | PEM files, re-read when they change |
| `limits.max_item_bytes` | `2 GiB` | Largest item |
| `limits.workspace_quota_bytes` | `20 GiB` | Quota of a new workspace |
| `limits.auth_failures_per_minute` | `10` | Per client address, then `RATE_LIMITED` |
| `staging.max_age_hours` | `24` | Age at which the janitor removes unfinished uploads |

## Environment

| Variable | Meaning |
|---|---|
| `PASSALONG_SERVER_CONFIG_FILE` | Path to `config.toml` |
| `PASSALONG_SERVER_LOG_LEVEL` | Overrides `server.log_level` |

The server needs no secret in its environment. API keys live hashed in the
control database; the TLS private key is a file.

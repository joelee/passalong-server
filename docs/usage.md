# Usage

> **Draft.** The commands below are the proposal of
> [IDEA-00001](ideas/00001-HTTPS_Server_Backend-r01.md); none exists yet.

## Commands

| Command | What it does |
|---|---|
| `passalong-server serve` | Runs the server in the foreground. systemd and Docker run this |
| `passalong-server init` | Writes a config file and creates the data directory |
| `passalong-server workspace create <name> [--quota 20GiB]` | Creates a workspace |
| `passalong-server workspace list` / `show <name>` | Items, bytes used, quota, encryption state, keys |
| `passalong-server workspace delete <name>` | Deletes a workspace, its items, and its keys, after confirmation |
| `passalong-server key create --workspace <name> --label <text> [--expires 90d \| --never] [--read-only]` | Prints the new key **once** |
| `passalong-server key list [--workspace <name>]` | Key id, label, role, created, expires, last used; never a secret |
| `passalong-server key extend <key id> --expires 90d` | Moves a key's expiry without redistributing it |
| `passalong-server key revoke <key id>` | Refuses the key from the next request on; keeps it in the audit trail |
| `passalong-server key delete <key id>` | Removes a key's record |
| `passalong-server key prune` | Deletes keys that expired or were revoked long ago |
| `passalong-server tls self-signed --host <name>` | Writes a certificate and key, and prints the pin for the client's `tls_pin` |
| `passalong-server tls fingerprint` | Prints the pin of the configured certificate |
| `passalong-server service install` / `remove` | The systemd system unit; needs root |
| `passalong-server check [--health]` | Validates the config, data directory, and TLS files; `--health` asks the running server |

With Docker, run them inside the container:

```text
docker compose exec server passalong-server key create --workspace home --label laptop --expires 90d
```

## On a device

passalong v0.3.0 and later: `passalong init`, choose `https`, and give the
URL and the key. See the client's documentation.

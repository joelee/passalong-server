# passalong-server

A self-hosted HTTPS server for [passalong](https://github.com/joelee/passalong),
the clipboard and file sharing tool: an alternative to keeping a passalong
store on an SSH server or in a local folder.

> **Status: it serves; no client speaks to it yet.** `passalong-server
> serve` answers every operation of the [API](docs/api/README.md) over TLS
> or behind a proxy, with API keys, workspaces, and the operator's commands
> ([usage](docs/usage.md)), installed with
> [Docker Compose](deploy/docker/README.md) or as a
> [systemd service](docs/usage.md#the-systemd-service). The passalong client
> learns the protocol in its v0.3.0. Start with the
> idea report,
> [IDEA-00001](docs/ideas/00001-HTTPS_Server_Backend-r04.md), and the
> [architecture draft](docs/architecture.md).

## What it will be

- **HTTPS** transport, with built-in TLS or behind your reverse proxy.
- **API keys** per device: expiring, revocable, optionally read-only.
- **Many workspaces** on one server, each a separate passalong store.
- **Zero knowledge** of encrypted workspaces: the server stores sealed items
  and never sees a key, the words, or plaintext.
- A plain **REST + JSON** API, with HTTP streams for content.
- One small Rust binary, as a **Docker** image or a **systemd** service,
  with its own **operations CLI**. Built from this repository: nothing is
  published while the licence is proprietary.

## Compatibility

| passalong-server | API | passalong client |
|---|---|---|
| 0.1.x (planned) | v1 | 0.3.0 and later (planned) |

## Documentation

- [Architecture](docs/architecture.md)
- [API](docs/api/README.md)
- [Configuration](docs/configuration.md)
- [Usage](docs/usage.md)
- [Developer guide](docs/developer-guide.md)
- [Backlog](docs/backlog.md)

## Licence

Proprietary, all rights reserved, until an open-source licence is chosen.
See [LICENSE](LICENSE).

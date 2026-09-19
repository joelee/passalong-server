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
  with its own **operations CLI**. Free software, under the GNU AGPL v3.

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

Copyright 2026 Joseph H Lee (@joelee).

passalong-server is free software: you can redistribute it and/or modify it
under the terms of the GNU Affero General Public License as published by
the Free Software Foundation, either version 3 of the License, or (at your
option) any later version (`AGPL-3.0-or-later`). It is distributed in the
hope that it will be useful, but WITHOUT ANY WARRANTY; without even the
implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
See [LICENSE](LICENSE).

The AGPL reaches network use: if you run a modified passalong-server for
others, you must offer them the source of your version.

The [passalong client](https://github.com/joelee/passalong) is a separate
work under Apache-2.0. The two meet only at the documented
[API](docs/api/README.md).

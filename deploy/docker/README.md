# passalong-server with Docker Compose

From a clone of this repository to a server with a first API key. The
commands under "Install", and the way files are put into a volume, are run
by `scripts/test-deploy.sh` (`just test-deploy`).

No image is published, so the first step builds one. Commands are run in
this folder.

## Install

```text
cp .env.sample .env
docker compose build
docker compose run --rm server init --data-dir /var/lib/passalong-server
docker compose run --rm server tls self-signed --host nas.example --ip 192.0.2.4
docker compose up -d
```

`--host` and `--ip` are what your devices will type to reach the server,
each as often as needed. `tls self-signed` prints the **pin** devices connect
by; `docker compose exec server passalong-server tls fingerprint` prints it
again. To use a certificate of your own, or a reverse proxy, see
[below](#your-own-certificate-or-a-reverse-proxy).

Then a workspace and a key for each device:

```text
docker compose exec server passalong-server workspace create home
docker compose exec server passalong-server key create --workspace home --label laptop
```

The key is shown once. Everything else is in [usage](../../docs/usage.md):
put `docker compose exec server` before any command there. Changes hold from
the server's next request on; nothing needs a restart.

`docker compose ps` shows `healthy` once the server is ready: the image asks
its own `/readyz`.

## Where things are

Two named volumes, not folders beside this file:

| Volume | In the container | Holds |
|---|---|---|
| `data` | `/var/lib/passalong-server` | Every workspace's items, and the control database with the API keys' hashes |
| `config` | `/etc/passalong-server` | `config.toml`, and `tls/` with the certificate and its private key |

A fresh named volume belongs to the server's user, uid 10001, so nothing has
to be `chown`ed. A host folder that Docker creates belongs to root: the
server could not write to it, and every command would refuse to run, because
the commands only run as the owner of the data. For the same reason, never
add `user:` to the compose file and never pass `--user` to `exec`.

To edit the configuration:

```text
docker compose exec -T server cat /etc/passalong-server/config.toml > config.toml
# edit config.toml, then:
docker compose exec -T server sh -c 'cat > /etc/passalong-server/config.toml' < config.toml
docker compose exec server passalong-server check
docker compose restart server
```

Files go in through `exec`, not `docker compose cp`: what `cp` puts there
belongs to root, and the server could not read a private key that does.

To copy the data out, with the server stopped so that the database is whole:

```text
docker compose stop
docker run --rm -v passalong-server_data:/from:ro -v "$PWD":/to debian:trixie-slim tar -C /from -czf /to/passalong-server-data.tgz .
docker compose start
```

`passalong-server_data` is the volume's name when this folder's project is
called `passalong-server`; `docker volume ls` shows yours.

If you want host folders all the same: create them first and give them to
uid 10001 (`sudo chown -R 10001 data config`), then replace `data:` and
`config:` in the service's `volumes` with `./data:` and `./config:`.

## Stopping, updating, removing

```text
docker compose stop              # lets requests in flight finish; up to 45 s
git pull && docker compose build && docker compose up -d
docker compose down              # removes the container, keeps both volumes
```

`docker compose down --volumes` deletes every item of every workspace, all
keys, and the TLS pair your devices have pinned. There is no undo.

## Your own certificate, or a reverse proxy

**A certificate of your own.** Put the pair into the `config` volume, as the
server's user and the key for its eyes only:

```text
docker compose exec -T server sh -c 'mkdir -p /etc/passalong-server/tls'
docker compose exec -T server sh -c 'cat > /etc/passalong-server/tls/cert.pem' < fullchain.pem
docker compose exec -T server sh -c 'umask 077; cat > /etc/passalong-server/tls/key.pem' < privkey.pem
```

The server reads a renewed pair within half a minute; no restart. Before the
server's first start there is no container to `exec` in; then each line
begins `docker compose run --rm -T --entrypoint sh server -c` instead.

**Let's Encrypt.** `docker compose exec server passalong-server tls
letsencrypt --docker --host pass.example.net` prints the `certbot` command and
a deploy hook that does the three lines above at every renewal. See
[usage](../../docs/usage.md#lets-encrypt), and mind what it says about pins.

**A reverse proxy that terminates TLS.** In `config.toml` set
`listen.mode = "plain"` and `listen.behind_proxy = true`, publish the port
to the proxy only, and have the proxy append the client's address to
`X-Forwarded-For`. See [configuration](../../docs/configuration.md#failed-authentications).

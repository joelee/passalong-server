#!/usr/bin/env bash
# Drives deploy/docker end to end, by the commands of deploy/docker/README.md:
# builds the image, initialises two fresh volumes, makes a TLS pair, starts
# the server, administers it with `compose exec` while it serves, uses it
# over HTTPS by pin from outside the container, and stops it. Fails unless
# the container ends with exit code 0 inside its grace period and its log
# holds no part of any API key. Leaves nothing behind: its containers, its
# volumes, and its image tag are removed whether it passes or not.
# `just test-deploy` runs it. Needs docker with compose, curl, sha256sum.
#
# Usage: scripts/test-deploy.sh [repository root]
set -euo pipefail

root="$(cd "${1:-$(dirname "$0")/..}" && pwd)"
work="$(mktemp -d)"
export PASSALONG_SERVER_PORT="${PASSALONG_DEPLOY_TEST_PORT:-18443}"
export PASSALONG_SERVER_IMAGE="passalong-server:deploy-test"
base="https://localhost:$PASSALONG_SERVER_PORT"

compose() { docker compose -f "$root/deploy/docker/compose.yaml" -p passalong-deploy-test "$@"; }
admin() { compose exec -T server passalong-server "$@"; }
started="$(date +%s)"
say() { printf '\n== [%3ss] %s\n' "$(( $(date +%s) - started ))" "$*"; }
fail() { printf 'FAILED: %s\n' "$*" >&2; exit 1; }

cleanup() {
    status=$?
    if [ "$status" -ne 0 ]; then compose logs --no-color 2>/dev/null | tail -40 >&2 || true; fi
    compose down --volumes --remove-orphans --timeout 50 >/dev/null 2>&1 || true
    docker image rm "$PASSALONG_SERVER_IMAGE" >/dev/null 2>&1 || true
    rm -rf "$work"
    exit "$status"
}
trap cleanup EXIT

say "build"
compose build --quiet

say "the image carries its terms and its dependencies' notices"
for file in LICENSE THIRD-PARTY-NOTICES; do
    compose run --rm -T --entrypoint sh server -c "test -s /usr/share/doc/passalong-server/$file" \
        || fail "the image has no $file"
done
compose run --rm -T --entrypoint sh server -c 'head -1 /usr/share/doc/passalong-server/LICENSE' \
    | grep -q "GNU AFFERO GENERAL PUBLIC LICENSE" || fail "the image's LICENSE is not the AGPL"

say "init, and a self-signed pair, before the server has ever run"
compose run --rm -T server init --data-dir /var/lib/passalong-server
compose run --rm -T server tls self-signed --host localhost --ip 127.0.0.1 >/dev/null
# Never over a pair that is there.
if compose run --rm -T server tls self-signed --host localhost >/dev/null 2>&1; then
    fail "a second tls self-signed wrote over the pair"
fi

say "up, and healthy"
compose up --detach
container="$(compose ps --quiet server)"
for _ in $(seq 60); do
    health="$(docker inspect --format '{{.State.Health.Status}}' "$container")"
    [ "$health" = healthy ] && break
    sleep 1
done
[ "$health" = healthy ] || fail "never healthy: $health"
[ "$(docker inspect --format '{{.Config.User}}' "$container")" = passalong-server ] \
    || fail "the container does not run as the image's user"

say "administered by compose exec while it serves (IDEA-00001 A-04)"
admin workspace create home >/dev/null
key="$(admin key create --workspace home --label laptop | grep -o 'pal_[0-9a-f]*_[0-9a-f]*')"
key_id="$(printf '%s' "$key" | cut -d_ -f2)"
secret="$(printf '%s' "$key" | cut -d_ -f3)"
pin="$(admin tls fingerprint | tr -d '\r\n')"
compose exec -T server cat /etc/passalong-server/tls/cert.pem > "$work/cert.pem"
# As root it is refused: the owner rule.
if compose exec -T --user 0 server passalong-server key list >/dev/null 2>&1; then
    fail "a command run as root was not refused"
fi

api() { curl -sS --fail-with-body --cacert "$work/cert.pem" --pinnedpubkey "sha256//${pin#sha256/}" -H "Authorization: Bearer $key" "$@"; }

say "used over HTTPS by pin, with the key made a moment ago"
api "$base/v1/viewer" | grep -q "\"id\":\"$key_id\"" || fail "the new key was not accepted"
printf 'hello from the deployment test' > "$work/content"
sha="$(sha256sum "$work/content" | cut -c1-64)"
size="$(wc -c < "$work/content" | tr -d ' ')"
id="$(printf '%08x' "$(date +%s)")-${sha:0:12}"
begun="$(api -X POST -H 'Content-Type: application/json' "$base/v1/uploads" \
    -d "{\"id\":\"$id\",\"meta\":{\"schema\":1,\"kind\":\"text\",\"sha256\":\"$sha\",\"size\":$size},\"size\":\"$size\"}")"
upload="$(printf '%s' "$begun" | grep -o '"uploadId":"[0-9a-f]*"' | cut -d'"' -f4)"
api -X PUT -H 'Content-Type: application/octet-stream' --data-binary "@$work/content" "$base/v1/uploads/$upload/content"
api -X POST "$base/v1/uploads/$upload/commit" | grep -q '"created":true' || fail "the upload was not committed"
api "$base/v1/item-ids" | grep -q "$id" || fail "the item is not listed"
api "$base/v1/items/$id/content" | cmp -s - "$work/content" || fail "the content came back changed"
admin workspace show home | grep -Eq '^items +1' || fail "the CLI does not see what the server stored"

say "another pin is refused"
if curl -sS --cacert "$work/cert.pem" --pinnedpubkey "sha256//AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=" "$base/healthz" >/dev/null 2>&1; then
    fail "curl connected by a wrong pin"
fi

say "a key revoked by compose exec is refused at the next request"
admin key revoke "$key_id" >/dev/null
code="$(curl -sS -o "$work/refused" -w '%{http_code}' --cacert "$work/cert.pem" --pinnedpubkey "sha256//${pin#sha256/}" -H "Authorization: Bearer $key" "$base/v1/viewer")"
[ "$code" = 401 ] && grep -q KEY_REVOKED "$work/refused" || fail "a revoked key got $code: $(cat "$work/refused")"

say "the configuration is edited the way the README says"
admin_sh() { compose exec -T server sh -c "$1"; }
compose exec -T server cat /etc/passalong-server/config.toml > "$work/config.toml"
sed -i 's/^auth_failures_per_minute = .*/auth_failures_per_minute = 7/' "$work/config.toml"
admin_sh 'cat > /etc/passalong-server/config.toml' < "$work/config.toml"
admin check >/dev/null || fail "the edited configuration does not check"
owner="$(admin_sh 'stat -c %u /etc/passalong-server/config.toml' | tr -d '\r\n')"
[ "$owner" = 10001 ] || fail "the configuration belongs to $owner"

say "it survives a restart with its data"
compose restart --timeout 50 server >/dev/null
for _ in $(seq 60); do
    admin check --health >/dev/null 2>&1 && break
    sleep 1
done
admin workspace show home | grep -Eq '^items +1' || fail "the item did not survive a restart"

say "stopped: exit code 0, inside the grace period, and no key in the log"
began="$(date +%s)"
compose stop >/dev/null
took="$(( $(date +%s) - began ))"
[ "$took" -lt 45 ] || fail "stopping took $took seconds"
exit_code="$(docker inspect --format '{{.State.ExitCode}}' "$container")"
[ "$exit_code" = 0 ] || fail "the server exited with $exit_code"
compose logs --no-color > "$work/log"
grep -q "request=" "$work/log" || fail "the log holds no request line"
grep -q "key=$key_id" "$work/log" || fail "the log does not name the key id"
for part in "$key" "$secret" "${secret:0:16}" Bearer; do
    if grep -q -- "$part" "$work/log"; then fail "the log holds part of a key: $part"; fi
done

say "passed"

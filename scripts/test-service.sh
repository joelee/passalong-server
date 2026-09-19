#!/usr/bin/env bash
# `passalong-server service install` and `service remove` on a real systemd:
# a throwaway container with systemd as PID 1, in which the commands run as
# root and do to that host what they would do to yours. Checks that the
# hardened unit starts, serves over TLS, is administered as the service's
# user, stops cleanly inside its timeout, and is removed without its data.
# Also runs `systemd-analyze verify` and prints `systemd-analyze security`.
# Leaves nothing behind. `just test-service` runs it. Needs Docker 28 or
# later, for `writable-cgroups`.
#
# The container is NOT privileged, and must never be made so. A privileged
# container's systemd applies its own sysctl settings, and some of those,
# `kernel.core_pattern` among them, are one value for the whole kernel: the
# first version of this script changed the core pattern of the machine it ran
# on. Without `--privileged`, Docker keeps /proc/sys read-only; the image
# sets no sysctl besides; and this script compares the host's kernel
# settings before and after and fails if one moved.
#
# Usage: scripts/test-service.sh [repository root]
set -euo pipefail

root="$(cd "${1:-$(dirname "$0")/..}" && pwd)"
work="$(mktemp -d)"
server_image="passalong-server:service-test"
host_image="passalong-server-systemd-host:service-test"
host="passalong-service-test"
unit=passalong-server.service

started="$(date +%s)"
say() { printf '\n== [%3ss] %s\n' "$(( $(date +%s) - started ))" "$*"; }
fail() { printf 'FAILED: %s\n' "$*" >&2; exit 1; }
in_host() { docker exec "$host" "$@"; }
as_service() { in_host sudo -u passalong-server passalong-server "$@"; }

cleanup() {
    status=$?
    if [ "$status" -ne 0 ]; then
        in_host journalctl -u "$unit" --no-pager -n 40 >&2 2>/dev/null || true
    fi
    docker rm -f "$host" >/dev/null 2>&1 || true
    docker rm -f "$host-binary" >/dev/null 2>&1 || true
    docker image rm "$server_image" "$host_image" >/dev/null 2>&1 || true
    rm -rf "$work"
    exit "$status"
}
trap cleanup EXIT

docker_major="$(docker version --format '{{.Server.Version}}' | cut -d. -f1)"
if [ "${docker_major:-0}" -lt 28 ] 2>/dev/null; then
    fail "this needs Docker 28 or later, for writable cgroups in a container that is not privileged; this daemon is $(docker version --format '{{.Server.Version}}'). It is never run privileged instead"
fi

say "build: the server's image, for its binary, and a host with systemd"
docker build --quiet -t "$server_image" "$root" >/dev/null
docker build --quiet -t "$host_image" -f "$root/deploy/test/systemd.Dockerfile" "$root/deploy/test" >/dev/null
docker create --name "$host-binary" "$server_image" >/dev/null
docker cp "$host-binary:/usr/local/bin/passalong-server" "$work/passalong-server"

say "boot"
host_settings() { cat /proc/sys/kernel/core_pattern /proc/sys/kernel/pid_max /proc/sys/fs/protected_* 2>/dev/null; }
host_settings > "$work/settings-before"
# What systemd needs and no more: to mount (its units' sandboxes are mount
# namespaces) and a cgroup subtree of its own to write to.
docker run --detach --name "$host" \
    --cap-add SYS_ADMIN --security-opt seccomp=unconfined --security-opt apparmor=unconfined \
    --security-opt writable-cgroups=true --cgroupns=private \
    --tmpfs /run --tmpfs /run/lock --tmpfs /tmp "$host_image" >/dev/null
if in_host sh -c 'echo probe > /proc/sys/kernel/core_pattern' 2>/dev/null; then
    fail "this container can write the host's kernel settings; refusing to go on"
fi
for _ in $(seq 60); do
    state="$(in_host systemctl is-system-running 2>/dev/null || true)"
    case "$state" in running|degraded) break ;; esac
    sleep 1
done
case "$state" in running|degraded) ;; *) fail "systemd did not come up: $state" ;; esac
# From a build directory, as an operator would: not from its final place.
in_host mkdir -p /root/build
docker cp "$work/passalong-server" "$host:/root/build/passalong-server"

say "service install, on an empty host"
in_host /root/build/passalong-server service install --host localhost --ip 127.0.0.1 | tee "$work/installed"
grep -q 'tls_pin = "sha256/' "$work/installed" || fail "no pin was printed"
grep -q "enabled and running" "$work/installed" || fail "it does not say it runs"
if grep -q "PRIVATE KEY" "$work/installed"; then fail "the key was printed"; fi
pin="$(grep -o 'sha256/[^"]*' "$work/installed")"

say "active, healthy, and everything the service's own"
for _ in $(seq 30); do
    as_service check --health >/dev/null 2>&1 && break
    sleep 1
done
[ "$(in_host systemctl is-active "$unit")" = active ] || fail "the unit is not active"
[ "$(in_host systemctl is-enabled "$unit")" = enabled ] || fail "the unit is not enabled"
as_service check --health
as_service check >/dev/null || fail "check finds fault with the installation"
for path in /var/lib/passalong-server /var/lib/passalong-server/control.sqlite \
    /etc/passalong-server /etc/passalong-server/config.toml /etc/passalong-server/tls/key.pem; do
    [ "$(in_host stat -c %U "$path")" = passalong-server ] || fail "$path is not the service's"
done
[ "$(in_host stat -c %a /etc/passalong-server/tls/key.pem)" = 600 ] || fail "the key is readable by others"
[ "$(in_host stat -c %a /etc/passalong-server)" = 700 ] || fail "systemd changed the configuration directory's mode"
# As root the commands refuse, which is the rule `service` alone is exempt from.
if in_host passalong-server key list >/dev/null 2>&1; then fail "a command run as root was not refused"; fi

say "used over TLS by pin"
as_service workspace create home >/dev/null
key="$(as_service key create --workspace home --label laptop | grep -o 'pal_[0-9a-f]*_[0-9a-f]*')"
in_host curl -sS --fail-with-body --cacert /etc/passalong-server/tls/cert.pem \
    --pinnedpubkey "sha256//${pin#sha256/}" -H "Authorization: Bearer $key" \
    https://localhost:8443/v1/viewer | grep -q '"label":"laptop"' || fail "the server did not answer by pin"

say "systemd-analyze"
in_host systemd-analyze verify "/etc/systemd/system/$unit" 2>&1 | tee "$work/verify"
if grep -q "$unit" "$work/verify"; then fail "systemd-analyze verify finds fault with the unit"; fi
in_host systemd-analyze security "$unit" --no-pager | tail -1

say "a second install changes nothing"
in_host /root/build/passalong-server service install --host other.example > "$work/again"
if grep -Eq '^(wrote|installed|made|ran systemd-sysusers|ran systemctl (daemon-reload|restart))' "$work/again"; then
    cat "$work/again"; fail "the second install changed something"
fi
[ "$(as_service tls fingerprint | tr -d '\r\n')" = "$pin" ] || fail "the pair was replaced"

say "stop: clean, and inside the unit's timeout"
began="$(date +%s)"
in_host systemctl stop "$unit"
took="$(( $(date +%s) - began ))"
[ "$took" -lt 45 ] || fail "stopping took $took seconds"
result="$(in_host systemctl show "$unit" -p Result -p ExecMainStatus | tr '\n' ' ')"
[ "$result" = "Result=success ExecMainStatus=0 " ] || fail "stopped with: $result"
if in_host journalctl -u "$unit" --no-pager | grep -q "${key##*_}"; then fail "the journal holds the key"; fi
in_host systemctl start "$unit"

say "service remove: the unit goes, the data stays"
in_host passalong-server service remove | tee "$work/removed"
[ "$(in_host systemctl is-active "$unit" || true)" = inactive ] || fail "it still runs"
if in_host test -e "/etc/systemd/system/$unit"; then fail "the unit file is still there"; fi
for path in /var/lib/passalong-server/control.sqlite /etc/passalong-server/config.toml \
    /etc/passalong-server/tls/key.pem /usr/local/bin/passalong-server; do
    in_host test -e "$path" || fail "$path was removed"
done
in_host passalong-server service remove | grep -q "nothing to remove" || fail "a second remove is an error"

say "and installed again over what stayed: the same pair, the same keys"
in_host passalong-server service install >/dev/null
for _ in $(seq 30); do
    as_service check --health >/dev/null 2>&1 && break
    sleep 1
done
[ "$(as_service tls fingerprint | tr -d '\r\n')" = "$pin" ] || fail "the pair did not survive"
as_service key list | grep -q laptop || fail "the key did not survive"

host_settings > "$work/settings-after"
cmp -s "$work/settings-before" "$work/settings-after" || fail "a kernel setting of this machine changed during the test"

say "passed"

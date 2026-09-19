#!/usr/bin/env bash
# Makes what a release offers for download, from a binary that is built
# already. The release workflow calls it; `just test-release-archive` calls
# it on this machine, so it is tried before a tag ever is.
#
#   release-archive.sh archive <version> <arch> <binary> <out dir>
#       writes <out dir>/passalong-server-v<version>-linux-<arch>.tar.gz with
#       the binary, LICENSE, THIRD-PARTY-NOTICES, README.md, and the systemd
#       unit and sysusers files. <arch> is amd64 or arm64.
#   release-archive.sh sums <out dir>
#       writes <out dir>/SHA256SUMS over every archive there.
#   release-archive.sh verify <out dir>
#       checks the sums, unpacks every archive, and checks what is in it; runs
#       the binary's --version where this machine can run it.
#
# The AGPL travels with the binary: an archive without LICENSE and
# THIRD-PARTY-NOTICES is not made.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
fail() { printf 'error: %s\n' "$*" >&2; exit 1; }
contents=(passalong-server LICENSE THIRD-PARTY-NOTICES README.md passalong-server.service passalong-server.sysusers)

case "${1:-}" in
archive)
    [ $# -eq 5 ] || fail "usage: release-archive.sh archive <version> <arch> <binary> <out dir>"
    version="$2" arch="$3" binary="$4" out="$5"
    printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || fail "'$version' is not MAJOR.MINOR.PATCH"
    case "$arch" in amd64|arm64) ;; *) fail "'$arch' is neither amd64 nor arm64" ;; esac
    [ -x "$binary" ] || fail "$binary is not an executable file"
    name="passalong-server-v$version-linux-$arch"
    stage="$(mktemp -d)"; trap 'rm -rf "$stage"' EXIT
    mkdir -p "$stage/$name" "$out"
    install -m 0755 "$binary" "$stage/$name/passalong-server"
    for file in LICENSE THIRD-PARTY-NOTICES README.md docs/service/passalong-server.service docs/service/passalong-server.sysusers; do
        [ -s "$root/$file" ] || fail "$file is missing: no archive without it"
        install -m 0644 "$root/$file" "$stage/$name/"
    done
    # The same bytes from the same input: sorted, no owner, no build time.
    tar --sort=name --owner=0 --group=0 --numeric-owner --mtime='2026-01-01 00:00:00Z' \
        -C "$stage" -cf - "$name" | gzip -n -9 > "$out/$name.tar.gz"
    echo "$out/$name.tar.gz"
    ;;
sums)
    [ $# -eq 2 ] || fail "usage: release-archive.sh sums <out dir>"
    cd "$2"
    ls passalong-server-v*.tar.gz >/dev/null 2>&1 || fail "no archive in $2"
    sha256sum passalong-server-v*.tar.gz > SHA256SUMS
    cat SHA256SUMS
    ;;
verify)
    [ $# -eq 2 ] || fail "usage: release-archive.sh verify <out dir>"
    cd "$2"
    sha256sum --check --strict SHA256SUMS
    for archive in passalong-server-v*.tar.gz; do
        grep -q " $archive\$" SHA256SUMS || fail "$archive is not in SHA256SUMS"
        name="${archive%.tar.gz}"
        unpacked="$(mktemp -d)"
        tar -xzf "$archive" -C "$unpacked"
        for file in "${contents[@]}"; do
            [ -s "$unpacked/$name/$file" ] || fail "$archive has no $file"
        done
        [ "$(find "$unpacked/$name" -type f | wc -l)" -eq "${#contents[@]}" ] || fail "$archive holds something unexpected"
        head -1 "$unpacked/$name/LICENSE" | grep -q "GNU AFFERO GENERAL PUBLIC LICENSE" || fail "$archive: LICENSE is not the AGPL"
        version="${name#passalong-server-v}"; version="${version%%-linux-*}"
        case "$name" in
        *-"$(uname -m | sed 's/x86_64/amd64/; s/aarch64/arm64/')")
            said="$("$unpacked/$name/passalong-server" --version)"
            [ "$said" = "passalong-server $version" ] || fail "$archive: the binary says '$said'"
            "$unpacked/$name/passalong-server" --help | grep -q "https://github.com/joelee/passalong-server" \
                || fail "$archive: the binary's help does not name the source"
            ;;
        esac
        rm -rf "$unpacked"
        echo "ok: $archive"
    done
    ;;
*)
    fail "usage: release-archive.sh archive|sums|verify ..."
    ;;
esac

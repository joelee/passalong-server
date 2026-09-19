# passalong-server task runner. Every quality gate here wraps the exact
# commands mandated by AGENTS.md; pre-commit and CI call these recipes.

set shell := ["bash", "-euo", "pipefail", "-c"]

image := "passalong-server:dev"

# List available recipes
default:
    @just --list

# Install developer tooling: llvm-tools-preview, cargo-llvm-cov, cargo-deny, actionlint (needs Go)
setup:
    rustup component add llvm-tools-preview
    command -v cargo-llvm-cov >/dev/null || cargo install --locked cargo-llvm-cov
    command -v cargo-deny >/dev/null || cargo install --locked cargo-deny
    command -v cargo-about >/dev/null || cargo install --locked cargo-about --features cli
    command -v actionlint >/dev/null || ! command -v go >/dev/null || GOBIN="$HOME/.local/bin" go install github.com/rhysd/actionlint/cmd/actionlint@v1.7.12

# Format all code in place
fmt:
    cargo fmt --all

# Check formatting
fmt-check:
    cargo fmt --all -- --check

# Lint with clippy, warnings are errors
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Run unit and integration tests (Docker-backed tests are excluded)
test:
    cargo test --workspace --all-targets --all-features

# Line coverage gate (>= 80%)
coverage:
    cargo llvm-cov --workspace --all-features --fail-under-lines 80 --summary-only

# Supply-chain audit: advisories, licences, bans, and sources (deny.toml)
audit:
    cargo deny check

# Lint the GitHub Actions workflows, with a local actionlint or its image
lint-workflows:
    if command -v actionlint >/dev/null; then actionlint; else docker run --rm -v "$PWD:/repo" -w /repo rhysd/actionlint:1.7.12; fi

# Build the workspace from the lockfile
build:
    cargo build --workspace --all-features --locked

# Check Markdown links: relative targets and headings (scripts/check-links.sh)
links:
    scripts/check-links.sh

# All mandated checks: format, lint, links, tests, coverage, build
check: fmt-check lint links test coverage build

# Full CI pipeline: all checks, then the audit, workflows, and the image
ci: check audit notices-check lint-workflows test-release-archive docker-build test-deploy test-service

# Build the container image
docker-build:
    docker build -t {{image}} .

# deploy/docker end to end, by the commands of its README
test-deploy:
    scripts/test-deploy.sh

# `service install` and `service remove` on a real systemd, in a throwaway container
test-service:
    scripts/test-service.sh

# Write THIRD-PARTY-NOTICES: the licences of every crate the binary links.
# Offline: a crate's licence is read from its own files, never asked of a
# service, so the file is the same on every machine.
notices:
    cargo about generate --frozen --fail -o THIRD-PARTY-NOTICES about.hbs

# Fail when THIRD-PARTY-NOTICES is not what `just notices` would write
notices-check:
    #!/usr/bin/env bash
    set -euo pipefail
    fresh="$(mktemp)"; trap 'rm -f "$fresh"' EXIT
    cargo about generate --frozen --fail -o "$fresh" about.hbs
    if ! cmp -s "$fresh" THIRD-PARTY-NOTICES; then
        echo "error: THIRD-PARTY-NOTICES is stale or missing; run \`just notices\` and commit it" >&2
        exit 1
    fi

# What a release offers for download, made and verified on this machine
test-release-archive:
    #!/usr/bin/env bash
    set -euo pipefail
    # As the release builds it: no features, so no fault injection.
    cargo build --release --locked -p passalong-server
    out="$(mktemp -d)"; trap 'rm -rf "$out"' EXIT
    version="$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"name":"passalong-server","version":"\([^"]*\)".*/\1/p')"
    arch="$(uname -m | sed 's/x86_64/amd64/; s/aarch64/arm64/')"
    scripts/release-archive.sh archive "$version" "$arch" target/release/passalong-server "$out"
    scripts/release-archive.sh sums "$out"
    scripts/release-archive.sh verify "$out"
    # The same input gives the same bytes.
    first="$(sha256sum "$out"/*.tar.gz | cut -c1-64)"
    scripts/release-archive.sh archive "$version" "$arch" target/release/passalong-server "$out" >/dev/null
    [ "$first" = "$(sha256sum "$out"/*.tar.gz | cut -c1-64)" ] || { echo "error: the archive is not reproducible" >&2; exit 1; }
    # And what is not a release is refused.
    ! scripts/release-archive.sh archive "$version" riscv target/release/passalong-server "$out" 2>/dev/null
    ! scripts/release-archive.sh archive 1.0 "$arch" target/release/passalong-server "$out" 2>/dev/null

# Run the CLI, e.g. `just run key list`
run *ARGS:
    cargo run -p passalong-server -- {{ARGS}}

# Planned recipes, added by the plans that need them (docs/backlog.md):
#   test-client       a released passalong client against this server

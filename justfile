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
ci: check audit lint-workflows docker-build

# Build the container image
docker-build:
    docker build -t {{image}} .

# Run the CLI, e.g. `just run key list`
run *ARGS:
    cargo run -p passalong-server -- {{ARGS}}

# Planned recipes, added by the plans that need them (docs/backlog.md):
#   test-integration  the built image, driven over HTTPS
#   test-client       a released passalong client against this server
#   test-deploy       deploy/docker end to end, as the client's does
#   openapi           export docs/api/openapi.json and fail on drift

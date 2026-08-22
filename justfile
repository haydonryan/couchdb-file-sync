set positional-arguments

default:
    @just --list

build:
    cargo build --workspace

check:
    cargo check --workspace
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings -W clippy::pedantic -W clippy::nursery
    cargo audit
    cargo deny check all
    cargo test --workspace
    cargo test --workspace --all-features
    cargo test --workspace --no-default-features

install:
    cargo install --path . --bin couchdb-file-sync --target-dir "${CARGO_TARGET_DIR:-target}"

run *args:
    cargo run --bin couchdb-file-sync -- {{args}}

test:
    #!/usr/bin/env bash
    set -euo pipefail
    if command -v cargo-nextest >/dev/null 2>&1; then
      cargo nextest run --workspace --no-fail-fast
    else
      cargo test --workspace
    fi

pre-commit:
    #!/usr/bin/env bash
    set -euo pipefail
    before_fmt_diff="$(mktemp)"
    after_fmt_diff="$(mktemp)"
    trap 'rm -f "$before_fmt_diff" "$after_fmt_diff"' EXIT
    git diff --name-only -- . >"$before_fmt_diff"
    cargo fmt --all
    git diff --name-only -- . >"$after_fmt_diff"
    if ! cmp -s "$before_fmt_diff" "$after_fmt_diff"; then
      echo "cargo fmt updated files. Review and stage the formatting changes, then commit again." >&2
      exit 1
    fi
    cargo clippy --workspace --all-targets --all-features -- -D warnings -W clippy::pedantic -W clippy::nursery
    cargo audit
    cargo deny check all
    cargo test
    ./scripts/scan-staged-secrets.sh


docker-deploy:
    COMPOSE_FILE=docker-compose.yml
    if [ -f compose.yaml ]; then
    COMPOSE_FILE=compose.yaml
    elif [ -f compose.yml ]; then
    COMPOSE_FILE=compose.yml
    elif [ -f docker-compose.yaml ]; then
    COMPOSE_FILE=docker-compose.yaml
    fi
    if command -v docker-compose >/dev/null 2>&1; then
    docker-compose -f "$COMPOSE_FILE" up -d --build
    else
    docker compose -f "$COMPOSE_FILE" up -d --build
    fi

benchmark:
    ./scripts/benchmark.sh

dep-bump:
    ./scripts/update-dependencies.sh

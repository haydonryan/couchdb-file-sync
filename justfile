set positional-arguments

default:
    @just --list

build:
    cargo build --workspace

check:
    cargo check --workspace
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo audit
    cargo deny check all
    cargo test --workspace
    cargo test --workspace --all-features
    cargo test --workspace --no-default-features

install:
    cargo install --path . --bin couchdb-file-sync

run *args:
    cargo run --bin couchdb-file-sync -- {{args}}

test:
    cargo test --workspace



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

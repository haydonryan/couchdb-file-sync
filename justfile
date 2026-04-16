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

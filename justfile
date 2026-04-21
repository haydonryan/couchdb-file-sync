build:
    cargo build

check:
    cargo fmt --all -- --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo audit
    cargo deny check all
    cargo test

install:
    cargo install --path . --root "${XDG_BIN_HOME:-$HOME}/.local"

pre-commit:
    ./scripts/scan-staged-secrets.sh
    cargo fmt --all
    cargo clippy --all-targets --all-features -- -D warnings
    cargo audit
    cargo deny check all
    cargo test

release *args:
    git pull --rebase
    cargo release {{args}}

run *args:
    cargo run -- {{args}}

test:
    cargo test

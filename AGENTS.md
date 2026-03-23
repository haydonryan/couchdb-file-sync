# AGENTS.md

After any change in this repository (edit, commit, or command that modifies files), always do the following in this order:

1. Run `cargo test` and fix any issues.
2. Run `cargo fmt` and fix any issues.
3. Run `cargo clippy --all-targets --all-features -- -D warnings` and fix any issues.
4. Run `cargo audit` and fix any issues.
5. Run `cargo deny check` and fix any issues.
6. Check whether `README.md` needs updating; update it if necessary.

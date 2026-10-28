#!/usr/bin/env bash
set -euo pipefail

# Regression test for the pre-commit staged-snapshot formatting check
# (see scripts/verify-staged-formatting.sh).
#
# Verifies that a commit whose staged snapshot is unformatted is rejected, while
# partially staged (unstaged) edits are preserved and the index is left
# untouched.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
VERIFY="$SCRIPT_DIR/verify-staged-formatting.sh"

fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT
cd "$fixture"

git init -q
git config user.email "test@example.com"
git config user.name "Test"

# Minimal Rust workspace (formatting check needs no dependencies or network).
cat > Cargo.toml <<'EOF'
[package]
name = "fixture"
version = "0.1.0"
edition = "2021"
EOF
mkdir -p src

printf 'fn main(){\nprintln!("hi");\n}\n' > src/main.rs
git add .
git commit -qm "initial"

# Intentionally unformatted STAGED content for a tracked Rust file:
printf 'fn staged(){\nprintln!("staged");\n}\n' > src/main.rs
git add src/main.rs

# Plus an UNSTAGED edit appended in the working tree. It must survive the hook
# run neither staged nor lost.
unstaged_marker="// unstaged edit that must not be staged or lost"
printf '\n%s\n' "$unstaged_marker" >> src/main.rs

# Sanity: the file is partially staged (index != HEAD and index != worktree).
if git diff --cached --quiet; then
  echo "FAIL: fixture has no staged change" >&2
  exit 1
fi
if git diff --quiet; then
  echo "FAIL: fixture has no unstaged change" >&2
  exit 1
fi

before_index="$(git rev-parse :src/main.rs)"

# Run the actual pre-commit staged-snapshot check (the fix under test).
if "$VERIFY"; then
  echo "FAIL: staged snapshot with unformatted content was accepted" >&2
  exit 1
fi

# The unstaged edit must still be present and unstaged.
if ! grep -qF "$unstaged_marker" src/main.rs; then
  echo "FAIL: unstaged edit was lost" >&2
  exit 1
fi
if git diff --quiet; then
  echo "FAIL: unstaged edit was staged" >&2
  exit 1
fi
# The index must be unchanged.
if [ "$before_index" != "$(git rev-parse :src/main.rs)" ]; then
  echo "FAIL: index changed during check" >&2
  exit 1
fi

echo "PASS: staged-snapshot formatting check rejects unformatted staged content"

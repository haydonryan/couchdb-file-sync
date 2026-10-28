#!/usr/bin/env bash
set -euo pipefail

# Verify that the STAGED snapshot (what `git commit` would actually record) is
# rustfmt-clean, without touching the index or the working tree.
#
# The old pre-commit compared `git diff --name-only` before/after `cargo fmt`.
# That only detects formatting that changes a file's *name* (i.e. a file that
# was not previously tracked). If `cargo fmt` reformats a file that is already
# staged, the filename list is unchanged, so the unformatted staged snapshot
# still gets committed — the hook silently validated a different snapshot than
# the commit.
#
# This helper instead materializes the index (git write-tree) into a temp dir
# and runs `cargo fmt --check` there. It therefore checks exactly the content
# that would be committed, while partially staged / unstaged edits are neither
# staged nor discarded.

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

# Nothing staged: the working-tree `cargo fmt` (run by the caller) is all that
# matters and there is no staged snapshot to validate.
if git diff --cached --quiet; then
  exit 0
fi

staged_archive="$(mktemp -d)"
trap 'rm -rf "$staged_archive"' EXIT

# git write-tree materializes exactly the index; git archive reproduces it as
# real files so `cargo fmt` can be run against the committed content.
if ! git archive "$(git write-tree)" | tar -x -C "$staged_archive"; then
  echo "pre-commit: failed to materialize the staged snapshot" >&2
  exit 1
fi

if ! (cd "$staged_archive" && cargo fmt --all -- --check); then
  echo "cargo fmt would change files in the staged snapshot." >&2
  echo "Stage the formatting changes, then commit again:" >&2
  echo "    git add <formatted files>" >&2
  exit 1
fi

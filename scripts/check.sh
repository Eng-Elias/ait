#!/bin/sh
# The local gate — the same checks CI runs, in one command.
# Run before pushing:  scripts/check.sh
set -eu
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
python3 scripts/check-dep-comments.py
scripts/check-src-hygiene.sh

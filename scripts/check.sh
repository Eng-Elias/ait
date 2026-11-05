#!/bin/sh
# The local gate — the same checks CI runs, in one command.
# Run before pushing:  scripts/check.sh
set -eu
# Minimal agent shells can lack cargo on PATH — the rustup shim lives
# here; a dead entry hurts nothing when it's already set.
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
python3 scripts/check-dep-comments.py
scripts/check-src-hygiene.sh
python3 scripts/check-e2e-sentinels.py
python3 scripts/check-e2e-fixtures.py

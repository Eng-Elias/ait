#!/bin/sh
# Debug scaffolding belongs in tests or local work, never merged
# source: `dbg!`, `println!`/`eprintln!` in library code, and
# `AIT_DEBUG*` env probes. `ait/src/main.rs` is exempt — print macros
# there ARE the CLI's stdout/stderr. Wired into CI's check job and
# `scripts/check.sh`.
set -eu

hits=$(grep -rnE 'dbg!|println!|eprintln!|AIT_DEBUG' crates/*/src --include='*.rs' \
    | grep -v 'ait/src/main\.rs:' || true)

if [ -n "$hits" ]; then
    echo "$hits"
    echo "check-src-hygiene: remove debug scaffolding from crates/*/src" >&2
    exit 1
fi

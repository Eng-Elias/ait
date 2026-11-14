#!/bin/sh
# The Linux-side Windows gate: cross-checks every crate that compiles
# for x86_64-pc-windows-msvc *without* an MSVC toolchain — the
# pure-Rust dependency trees (ait-config, ait-shell, ait-term,
# ait-testkit). The rest of the workspace pulls aws-lc-sys /
# libsqlite3-sys, which need lib.exe/NASM and only check on the
# windows-latest CI runners (docs/windows-verification.md).
#
# Run it on any change to the Windows seams: cfg(windows) arms, the
# spawn/injection chain, the ipc transport, the path seams.
set -eu
export PATH="$HOME/.cargo/bin:$PATH"

rustup target list --installed | grep -qx x86_64-pc-windows-msvc \
    || rustup target add x86_64-pc-windows-msvc
cargo check --target x86_64-pc-windows-msvc --all-targets \
    -p ait-config -p ait-shell -p ait-term -p ait-testkit

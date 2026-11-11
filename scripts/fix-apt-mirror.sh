#!/bin/sh
# apt on GitHub-hosted runners resolves through
# mirror+file:/etc/apt/apt-mirrors.txt plus explicit *.ubuntu.com
# sources — and every ubuntu.com host can fail DNS resolution at
# once: on 2026-11-11 azure.archive.ubuntu.com, archive.ubuntu.com and
# security.ubuntu.com all returned "Temporary failure resolving" for
# 2h+, while packages.microsoft.com, crates.io and github.com resolved
# fine. That outage killed the v1.0.0 musl release leg twice
# (`apt-get install musl-tools`) and the dev→main promotion CI's
# `apt-get install zsh fish` in both the check and e2e jobs.
#
# This repoints every apt source — mirror list, one-line .list and
# deb822 .sources entries — at kernel.org's Ubuntu archive mirror, a
# different domain carrying the same suites (the /ubuntu or
# /ubuntu-ports path tail is preserved), then warms the indexes with
# retries. Afterwards a plain `apt-get install` hits a working
# mirror — or is already a no-op.
#
#   scripts/fix-apt-mirror.sh
#
# Used by ci.yml's shell installs, release.yml's dist-injected musl
# build-setup step, and verify-install.sh's container preamble (the
# script is catted into that file's container.sh — it must end by
# falling through, not `exit 0`). Runs as root inside the container;
# sudo is only used when needed. Idempotent; no-op off apt systems.
set -eu

[ -d /etc/apt ] || exit 0

if [ "$(id -u)" -eq 0 ]; then
    SUDO=
else
    SUDO=sudo
fi

# Country mirrors too ([a-z]+\.) — but not esm.ubuntu.com, whose
# token-gated trees kernel.org does not mirror.
for f in /etc/apt/sources.list \
         /etc/apt/sources.list.d/*.list \
         /etc/apt/sources.list.d/*.sources; do
    [ -f "$f" ] || continue
    $SUDO sed -i -E \
        's#https?://([a-z]+\.)?(archive|security|ports)\.ubuntu\.com/#http://mirrors.edge.kernel.org/#g' \
        "$f"
done

# mirror+file: sources resolve through this list. kernel.org first;
# archive.ubuntu.com stays as a fallback line for suites the mirror
# might lag on.
if [ -f /etc/apt/apt-mirrors.txt ]; then
    printf '%s\n' \
        'http://mirrors.edge.kernel.org/ubuntu' \
        'http://archive.ubuntu.com/ubuntu' \
        | $SUDO tee /etc/apt/apt-mirrors.txt >/dev/null
fi

# Warm the index cache with retries.
attempt=1
until $SUDO apt-get -o Acquire::Retries=3 update; do
    attempt=$((attempt + 1))
    if [ "$attempt" -gt 3 ]; then
        echo "fix-apt-mirror: apt-get update failed after 3 attempts" >&2
        exit 1
    fi
    sleep 10
done

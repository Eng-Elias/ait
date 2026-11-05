#!/bin/sh
# Bring up (and tear down) a throwaway OpenSSH sshd container for the
# AIT_E2E_SSH-gated e2e — Spec R09-T8's contributor entry point.
#
#   scripts/ssh-e2e.sh up     build + start, then print the env exports
#   scripts/ssh-e2e.sh test   run the gated cargo test against it
#   scripts/ssh-e2e.sh down   stop + remove everything
#   scripts/ssh-e2e.sh run    up + test + down (the whole loop)
#
# What `test` needs is the env contract it prints; `run` handles the
# exports itself. No docker? `AIT_E2E_SSH=1 cargo test -p ait
# --test ssh -- --ignored` alone makes the test provision a local
# sshd instead — same gate, no container.

set -eu

NAME=ait-ssh-e2e
IMAGE=ait-ssh-e2e
STATE=${AIT_SSH_E2E_STATE:-/tmp/ait-ssh-e2e}

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

up() {
    mkdir -p "$STATE"
    chmod 700 "$STATE"
    if [ ! -f "$STATE/user_key" ]; then
        ssh-keygen -t ed25519 -f "$STATE/user_key" -N '' -q
    fi
    cp "$STATE/user_key.pub" "$repo_root/scripts/ssh-e2e/authorized_keys"

    docker build -q -t "$IMAGE" "$repo_root/scripts/ssh-e2e" >/dev/null
    docker rm -f "$NAME" >/dev/null 2>&1 || true
    docker run -d --name "$NAME" -p 127.0.0.1:0:22 "$IMAGE" >/dev/null

    # `docker port` prints the published endpoint — keep only the port.
    port=$(docker port "$NAME" 22/tcp | awk -F: '{print $NF}')
    cat >"$STATE/env" <<EOF
AIT_E2E_SSH=1
AIT_E2E_SSH_PORT=$port
AIT_E2E_SSH_KEY=$STATE/user_key
AIT_E2E_SSH_USER=testuser
AIT_E2E_SSH_HOST=127.0.0.1
EOF
    rm -f "$repo_root/scripts/ssh-e2e/authorized_keys"
    cat <<EOF
sshd up on 127.0.0.1:$port — export:
  . $STATE/env     (sh-compatible exports, or set the vars by hand)
EOF
}

down() {
    docker rm -f "$NAME" >/dev/null 2>&1 || true
    rm -rf "$STATE"
    rm -f "$repo_root/scripts/ssh-e2e/authorized_keys"
    echo "sshd down"
}

run_test() {
    [ -f "$STATE/env" ] || {
        echo "no env at $STATE/env — run 'up' first" >&2
        exit 1
    }
    set -a
    . "$STATE/env"
    set +a
    cd "$repo_root"
    cargo test -p ait --test ssh -- --ignored --nocapture
}

case "${1:-}" in
    up) up ;;
    down) down ;;
    test) run_test ;;
    run)
        up
        trap down EXIT
        run_test
        ;;
    *)
        echo "usage: $0 {up|down|test|run}" >&2
        exit 2
        ;;
esac

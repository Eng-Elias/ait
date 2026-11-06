#!/bin/sh
# §5.11 "verified install" gate — Spec R12 us-29, ticket R12-T5.
#
#   scripts/verify-install.sh [--native] <tag>
#
# Downloads the release's ait-installer.sh, runs it in a clean
# environment, and asserts `ait --version` prints exactly `ait <v>`
# where <v> is the tag minus a leading `v`. Exits non-zero naming what
# it got vs what it expected on any mismatch. It is both the
# maintainer's local check and the shared implementation behind
# .github/workflows/verify-install.yml's two legs:
#
#   default (docker)  runs everything inside a throwaway ubuntu:24.04
#                     container — the `scripts/ssh-e2e.sh` pattern.
#                     The image ships WITHOUT curl; the container first
#                     apt-installs curl + ca-certificates, which is the
#                     clean-machine simulation: a fresh user machine
#                     gains curl before the curl|sh one-liner can run.
#                     The installer fetch happens inside the container
#                     too, so the host needs nothing beyond docker.
#   --native          skips docker and runs the installer on the host.
#                     This is the macOS leg — GH macos runners have no
#                     docker — and exercises the darwin tarball.
#
# Env:
#   AIT_RELEASE_BASE_URL  where ait-installer.sh is fetched from;
#                         default
#                         https://github.com/Eng-Elias/ait/releases/download/<tag>
#   AIT_DOWNLOAD_URL      installer env var — overrides the artifact
#                         base URL baked into ait-installer.sh. Set
#                         both to a local `python3 -m http.server
#                         target/distrib/` to exercise a locally built
#                         release end to end.
#   AIT_INSTALL_DIR       installer env var — install PREFIX override;
#                         the binary lands in <dir>/bin (cargo-home
#                         layout) and the version check mirrors that.
#   AIT_GITHUB_TOKEN, AIT_NO_MODIFY_PATH, AIT_PRINT_VERBOSE,
#   AIT_PRINT_QUIET, INSTALLER_*   passed through to the installer.
#
# Loopback note (docker mode): 127.0.0.1/localhost inside a container
# is the container, not the host — loopback URLs in the env above are
# rewritten to host.docker.internal (mapped via --add-host) so a
# locally served target/distrib/ works without --network host.

set -eu

usage() {
    echo "usage: $0 [--native] <tag>" >&2
    exit 2
}

native=0
if [ "${1:-}" = "--native" ]; then
    native=1
    shift
fi
[ $# -eq 1 ] || usage
tag=$1

version=${tag#v}
export AIT_EXPECTED_VERSION="ait $version"
export AIT_RELEASE_BASE_URL=${AIT_RELEASE_BASE_URL:-https://github.com/Eng-Elias/ait/releases/download/$tag}

# The install + assert body shared by both legs. Written to a file so
# it runs verbatim on the host (--native) or as the tail of the
# container script (docker) — one implementation, two environments.
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cat >"$work/verify-inner.sh" <<'EOF'
set -eu
d=$(mktemp -d)
trap 'rm -rf "$d"' EXIT
curl -fsSL "$AIT_RELEASE_BASE_URL/ait-installer.sh" -o "$d/ait-installer.sh"
sh "$d/ait-installer.sh"
# Mirror the installer's own dir resolution (install-path CARGO_HOME):
# a forced AIT_INSTALL_DIR is a prefix — the binary lands in <dir>/bin.
if [ -n "${AIT_INSTALL_DIR:-}" ]; then
    install_dir=$AIT_INSTALL_DIR/bin
elif [ -n "${CARGO_HOME:-}" ]; then
    install_dir=$CARGO_HOME/bin
else
    install_dir=$HOME/.cargo/bin
fi
got=$("$install_dir/ait" --version) || {
    echo "verify-install: '$install_dir/ait --version' failed to run" >&2
    exit 1
}
if [ "$got" != "$AIT_EXPECTED_VERSION" ]; then
    echo "verify-install: expected '$AIT_EXPECTED_VERSION', got '$got'" >&2
    exit 1
fi
echo "verify-install: $got — install verified ($install_dir/ait)"
EOF

if [ "$native" = 1 ]; then
    command -v curl >/dev/null 2>&1 || {
        echo "verify-install: --native needs curl on the host" >&2
        exit 1
    }
    sh "$work/verify-inner.sh"
    exit 0
fi

command -v docker >/dev/null 2>&1 || {
    echo "verify-install: needs docker on the host (or --native)" >&2
    exit 1
}

# Rewrite loopback URLs for the container (see header note). sed over
# case-globbing keeps both the scheme and any port/path untouched.
rewrite_loopback() {
    printf '%s' "$1" | sed \
        -e 's|^\(https\?://\)127\.0\.0\.1|\1host.docker.internal|' \
        -e 's|^\(https\?://\)localhost|\1host.docker.internal|'
}
container_base=$(rewrite_loopback "$AIT_RELEASE_BASE_URL")
if [ "$container_base" != "$AIT_RELEASE_BASE_URL" ]; then
    echo "verify-install: loopback URL rewritten for the container: $container_base" >&2
fi

# --add-host host.docker.internal:host-gateway is a no-op where Docker
# already provides the name (Docker Desktop) and creates it on Linux —
# one flag covers both. Only set vars are forwarded into the container.
set -- --rm -i --add-host host.docker.internal:host-gateway \
    -e "AIT_RELEASE_BASE_URL=$container_base" \
    -e "AIT_EXPECTED_VERSION=$AIT_EXPECTED_VERSION"
for var in AIT_DOWNLOAD_URL AIT_INSTALL_DIR AIT_GITHUB_TOKEN \
    AIT_NO_MODIFY_PATH AIT_PRINT_VERBOSE AIT_PRINT_QUIET \
    INSTALLER_DOWNLOAD_URL INSTALLER_NO_MODIFY_PATH \
    INSTALLER_PRINT_VERBOSE INSTALLER_PRINT_QUIET; do
    eval "val=\${$var:-}"
    if [ -n "$val" ]; then
        case $var in
            *DOWNLOAD_URL)
                val=$(rewrite_loopback "$val")
                ;;
        esac
        set -- "$@" -e "$var=$val"
    fi
done

{
    cat <<'EOF'
set -eu
export DEBIAN_FRONTEND=noninteractive
# The image starts WITHOUT curl on purpose — installing it here is the
# clean-machine simulation (see this script's header).
apt-get update -qq
apt-get install -y -qq --no-install-recommends curl ca-certificates
EOF
    cat "$work/verify-inner.sh"
} >"$work/container.sh"

docker run "$@" ubuntu:24.04 sh -s <"$work/container.sh"

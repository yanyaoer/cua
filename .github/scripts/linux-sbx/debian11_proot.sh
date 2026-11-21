#!/usr/bin/env bash
# Run a command inside the Debian 11 build root under proot.
#
#   debian11_proot.sh --root DIR [--bind HOST:GUEST]... [--cwd GUEST] -- CMD...
#
# proot (ptrace, no privileges) stands in for the debian:11 container: it binds
# /proc, /dev and /sys, which a chroot cannot mount on these runners. The proot
# binary is pinned by sha256 and kept next to DIR.
#
# PATH inside has rustup's cargo bin and Node; HOME is /root.
set -euo pipefail

PROOT_URL=https://proot.gitlab.io/proot/bin/proot
PROOT_SHA256=90375de3807212b8f948ff98ed66020f7c9cf7ea447c8734f1af02a2643c8d26

ROOT=""
CWD="/"
BINDS=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --root) ROOT="$2"; shift 2 ;;
        --bind) BINDS+=(-b "$2"); shift 2 ;;
        --cwd) CWD="$2"; shift 2 ;;
        --) shift; break ;;
        -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
[[ -n $ROOT && $# -gt 0 ]] || { echo "error: --root and a command are required" >&2; exit 2; }

PROOT="$(dirname "$ROOT")/proot"
if ! echo "$PROOT_SHA256  $PROOT" | sha256sum -c - >/dev/null 2>&1; then
    curl -fsSL --retry 3 -o "$PROOT" "$PROOT_URL"
    echo "$PROOT_SHA256  $PROOT" | sha256sum -c - >/dev/null \
        || { echo "error: sha256 mismatch for $PROOT_URL" >&2; exit 1; }
    chmod +x "$PROOT"
fi

exec env -i \
    HOME=/root TERM=dumb LANG=C.UTF-8 \
    PATH=/root/.cargo/bin:/opt/node/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    CUA_TELEMETRY=0 DO_NOT_TRACK=1 DEBIAN_FRONTEND=noninteractive \
    ${CARGO_BUILD_JOBS:+CARGO_BUILD_JOBS=$CARGO_BUILD_JOBS} \
    ${CUA_DRIVER_GIT_SHA:+CUA_DRIVER_GIT_SHA=$CUA_DRIVER_GIT_SHA} \
    ${LINUX_SBX_APT_MIRROR:+LINUX_SBX_APT_MIRROR=$LINUX_SBX_APT_MIRROR} \
    ${RUSTUP_DIST_SERVER:+RUSTUP_DIST_SERVER=$RUSTUP_DIST_SERVER} \
    ${RUSTUP_UPDATE_ROOT:+RUSTUP_UPDATE_ROOT=$RUSTUP_UPDATE_ROOT} \
    ${LINUX_SBX_CARGO_MIRROR:+LINUX_SBX_CARGO_MIRROR=$LINUX_SBX_CARGO_MIRROR} \
    ${npm_config_registry:+npm_config_registry=$npm_config_registry} \
    PROOT_NO_SECCOMP="${PROOT_NO_SECCOMP:-}" \
    "$PROOT" -0 -r "$ROOT" -b /proc -b /dev -b /sys -b /etc/resolv.conf "${BINDS[@]}" -w "$CWD" "$@"

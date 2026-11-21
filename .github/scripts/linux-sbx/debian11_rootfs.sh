#!/usr/bin/env bash
# Build (or reuse) the Debian 11 build root for cluster_build.sh.
#
#   debian11_rootfs.sh --dir DIR [--cache-tar FILE]
#
# Run as root. The build hosts cannot pull debian:11 (no Docker Hub), so this
# script debootstraps
# bullseye into DIR and the build runs in it under proot (debian11_proot.sh),
# which binds /proc without mount privileges (rustup, rustc and cargo need
# /proc/self/exe). The result matches the debian:11 container of the GitHub
# workflow: glibc 2.31, the packages of apt_bootstrap.sh, Rust from
# rust-toolchain.toml, Node 22.
#
# Every download is pinned by sha256 below; debootstrap checks the Release
# files with the Debian archive keyring, and apt inside the root keeps checking
# them (apt_bootstrap.sh uses signed-by).
#
# --cache-tar FILE: when FILE exists and its stamp matches this script's inputs,
# unpack it instead of rebuilding; after a rebuild, write it (for the next
# build host's cache). A missing or stale cache only costs time.
#
# Exit 0 with DIR ready, 1 on failure.
set -euo pipefail

DIR=""
CACHE_TAR=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --dir) DIR="$2"; shift 2 ;;
        --cache-tar) CACHE_TAR="$2"; shift 2 ;;
        -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
[[ -n $DIR ]] || { echo "error: --dir is required" >&2; exit 2; }

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

SNAPSHOT=20260901T000000Z
DEBOOTSTRAP_URL=https://deb.debian.org/debian/pool/main/d/debootstrap/debootstrap_1.0.145.tar.gz
DEBOOTSTRAP_SHA256=7cc8f9e386ed8dfd4b062a3b9527f0f45176dfa1b065e7db270f837956516b20
KEYRING_URL=http://snapshot.debian.org/archive/debian/$SNAPSHOT/pool/main/d/debian-archive-keyring/debian-archive-keyring_2021.1.1+deb11u1_all.deb
KEYRING_SHA256=28ca7749ab7978f3c571732c3aa1c56e3ad1d5db3c915293763d4f6cb8fcce89
NODE_VERSION=v22.23.5
NODE_SHA256=cdeb8670a0c81f51b4dc4ac61919f9b1ad917b304bf8221eb939570663d240f8
RUST_TOOLCHAIN=1.97.1
MIRRORS=("http://snapshot.debian.org/archive/debian/$SNAPSHOT/" "https://deb.debian.org/debian/")
NODE_BASE=https://nodejs.org/dist
RUSTUP_SH=https://sh.rustup.rs

# Download proxies (cluster_build.sh --mirror-env; for build hosts where
# static.rust-lang.org and friends stall). Each is a plain proxy of the upstream URL; every
# download is still checked by the sha256 pins above, debootstrap and apt by the
# Debian keyring, so a mirror changes where bytes come from, not what is accepted.
#   LINUX_SBX_DEBIAN_MIRROR  deb.debian.org layout (…/debian, …/debian-security)
#   LINUX_SBX_NODE_MIRROR    nodejs.org/dist layout
#   LINUX_SBX_RUSTUP_MIRROR  static.rust-lang.org layout
if [[ -n ${LINUX_SBX_DEBIAN_MIRROR:-} ]]; then
    DEBOOTSTRAP_URL="$LINUX_SBX_DEBIAN_MIRROR/debian/pool/main/d/debootstrap/debootstrap_1.0.145.tar.gz"
    KEYRING_URL="$LINUX_SBX_DEBIAN_MIRROR/debian/pool/main/d/debian-archive-keyring/debian-archive-keyring_2021.1.1+deb11u1_all.deb"
    MIRRORS=("$LINUX_SBX_DEBIAN_MIRROR/debian/" "${MIRRORS[@]}")
fi
[[ -n ${LINUX_SBX_NODE_MIRROR:-} ]] && NODE_BASE="$LINUX_SBX_NODE_MIRROR"
[[ -n ${LINUX_SBX_RUSTUP_MIRROR:-} ]] && RUSTUP_SH="$LINUX_SBX_RUSTUP_MIRROR/rustup/rustup-init.sh"

# The cache stamp: this script plus the apt package policy it runs.
STAMP="$(cat "$0" "$HERE/apt_bootstrap.sh" | sha256sum | awk '{print $1}')"

fetch() { # url sha256 dest
    curl -fsSL --retry 3 -o "$3" "$1"
    echo "$2  $3" | sha256sum -c - >/dev/null || { echo "error: sha256 mismatch for $1" >&2; exit 1; }
}

if [[ -n $CACHE_TAR && -f $CACHE_TAR ]]; then
    rm -rf "$DIR"; mkdir -p "$DIR"
    if tar -xzf "$CACHE_TAR" -C "$DIR" && [[ "$(cat "$DIR/.linux-sbx-rootfs-stamp" 2>/dev/null)" == "$STAMP" ]]; then
        echo "rootfs: reused cache $CACHE_TAR (stamp $STAMP)"
        exit 0
    fi
    echo "rootfs: cache stale or unreadable; rebuilding"
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
start=$(date +%s)

fetch "$DEBOOTSTRAP_URL" "$DEBOOTSTRAP_SHA256" "$WORK/debootstrap.tgz"
tar -xzf "$WORK/debootstrap.tgz" -C "$WORK"
export DEBOOTSTRAP_DIR="$WORK/debootstrap"

command -v ar >/dev/null 2>&1 || yum install -y -q binutils >/dev/null
fetch "$KEYRING_URL" "$KEYRING_SHA256" "$WORK/keyring.deb"
(cd "$WORK" && ar x keyring.deb data.tar.xz && mkdir kr && tar -xJf data.tar.xz -C kr)
KEYRING="$WORK/kr/usr/share/keyrings/debian-archive-keyring.gpg"
[[ -s $KEYRING ]] || { echo "error: keyring missing from $KEYRING_URL" >&2; exit 1; }

ok=0
for mirror in "${MIRRORS[@]}"; do
    rm -rf "$DIR"
    # Second stage cannot mount /proc here (no CAP_SYS_ADMIN); debootstrap
    # warns and carries on, as the runner probe showed.
    if "$DEBOOTSTRAP_DIR/debootstrap" --arch=amd64 --variant=minbase --keyring="$KEYRING" \
        bullseye "$DIR" "$mirror" >"$WORK/debootstrap.log" 2>&1; then
        echo "rootfs: debootstrap from $mirror in $(( $(date +%s) - start ))s"
        ok=1; break
    fi
    echo "rootfs: debootstrap from $mirror failed:" >&2
    tail -20 "$WORK/debootstrap.log" >&2
done
[[ $ok -eq 1 ]] || exit 1
cat "$DIR/etc/debian_version"

cp /etc/resolv.conf "$DIR/etc/resolv.conf"
install -d "$DIR/tmp/linux-sbx"
cp "$HERE/apt_bootstrap.sh" "$DIR/tmp/linux-sbx/"
fetch "$NODE_BASE/$NODE_VERSION/node-$NODE_VERSION-linux-x64.tar.xz" "$NODE_SHA256" "$WORK/node.tar.xz"
install -d "$DIR/opt/node"
tar -xJf "$WORK/node.tar.xz" -C "$DIR/opt/node" --strip-components=1

"$HERE/debian11_proot.sh" --root "$DIR" -- /bin/sh -euc "
    sh /tmp/linux-sbx/apt_bootstrap.sh
    curl -fsSL --retry 3 --max-time 300 $RUSTUP_SH -o /tmp/rustup-init.sh
    sh /tmp/rustup-init.sh -y --no-modify-path --profile minimal \
        --default-toolchain $RUST_TOOLCHAIN --component rustfmt \
        --target x86_64-unknown-linux-gnu
    rm -f /tmp/rustup-init.sh
    apt-get clean
    ldd --version | head -1; rustc --version; cargo --version; node --version; npm --version
"
rm -rf "$DIR/tmp/linux-sbx"
echo "$STAMP" >"$DIR/.linux-sbx-rootfs-stamp"
echo "rootfs: ready in $(( $(date +%s) - start ))s"

if [[ -n $CACHE_TAR ]]; then
    mkdir -p "$(dirname "$CACHE_TAR")"
    tar -czf "$CACHE_TAR" -C "$DIR" . && ls -lh "$CACHE_TAR"
fi

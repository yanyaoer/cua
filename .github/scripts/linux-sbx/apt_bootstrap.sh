#!/bin/sh
# Install the Linux build prerequisites in a bare debian:11 container.
#
#   sh apt_bootstrap.sh [--root DIR] [--retry-delay SECONDS]
#
# Same policy as the `Install base tooling (container is bare)` step of
# .github/workflows/cd-rust-cua-driver.yml: bullseye is past support, so the
# package indexes come from a frozen, signed snapshot.debian.org view, and
# archive.debian.org (the same final packages, past Valid-Until by design) is
# the fallback when the snapshot is unreachable after retries.
#
# POSIX sh on purpose: it runs before anything else is installed.
#
#   --root DIR           Prefix for the files this script writes (apt.conf.d,
#                        sources.list, the lists directory; default: /). apt-get
#                        itself still reads the real /etc/apt: this is a seam for
#                        exercising the policy with a stub apt-get, not a chroot.
#   --retry-delay N      Seconds multiplied by the attempt number between apt
#                        retries (default: 20).
#
# Exit 0 when the packages are installed from either mirror, 1 otherwise.
set -eu

ROOT=""
DELAY=20

while [ $# -gt 0 ]; do
    case "$1" in
        --root) ROOT="${2%/}"; shift 2 ;;
        --retry-delay) DELAY="$2"; shift 2 ;;
        -h|--help) sed -n '2,21p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

SNAPSHOT=20260901T000000Z
PACKAGES="git ca-certificates curl python3 build-essential pkg-config libx11-dev libxi-dev libxtst-dev libxext-dev libwayland-dev libxkbcommon-dev"
SOURCES="$ROOT/etc/apt/sources.list"
LISTS="$ROOT/var/lib/apt/lists"

mkdir -p "$ROOT/etc/apt/apt.conf.d"
cat >"$ROOT/etc/apt/apt.conf.d/80-cua-retries" <<'EOF'
Acquire::Retries "5";
Acquire::http::Timeout "30";
Acquire::https::Timeout "30";
EOF

apt_retry() {
    n=0
    until "$@"; do
        n=$((n + 1))
        if [ "$n" -ge 3 ]; then return 1; fi
        echo "apt step failed (attempt $n of 3); retrying in $((n * DELAY))s" >&2
        sleep $((n * DELAY))
    done
}

apt_bootstrap() {
    # Ignore any additional image-provided sources for this bootstrap.
    # shellcheck disable=SC2086 # PACKAGES is a word list
    apt_retry apt-get -o Dir::Etc::sourceparts=- update &&
        apt_retry apt-get -o Dir::Etc::sourceparts=- install -y --no-install-recommends $PACKAGES
}

rm -f "$ROOT/etc/apt/sources.list.d/debian.sources"
cat >"$SOURCES" <<EOF
deb [check-valid-until=no signed-by=/usr/share/keyrings/debian-archive-keyring.gpg] http://snapshot.debian.org/archive/debian/$SNAPSHOT/ bullseye main
deb [check-valid-until=no signed-by=/usr/share/keyrings/debian-archive-keyring.gpg] http://snapshot.debian.org/archive/debian/$SNAPSHOT/ bullseye-updates main
deb [check-valid-until=no signed-by=/usr/share/keyrings/debian-archive-keyring.gpg] http://snapshot.debian.org/archive/debian-security/$SNAPSHOT/ bullseye-security main
EOF

if apt_bootstrap; then
    echo "apt mirror used: snapshot.debian.org $SNAPSHOT"
    exit 0
fi

echo "::warning::snapshot.debian.org failed after retries; falling back to archive.debian.org"
rm -rf "$LISTS"
mkdir -p "$LISTS"
cat >"$SOURCES" <<'EOF'
deb [check-valid-until=no] http://archive.debian.org/debian bullseye main
deb [check-valid-until=no] http://archive.debian.org/debian bullseye-updates main
deb [check-valid-until=no] http://archive.debian.org/debian-security bullseye-security main
EOF
if apt_bootstrap; then
    echo "apt mirror used: archive.debian.org (snapshot fallback)"
    exit 0
fi

echo "::error::apt bootstrap failed on both snapshot.debian.org and archive.debian.org" >&2
exit 1

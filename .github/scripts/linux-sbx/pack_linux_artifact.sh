#!/usr/bin/env bash
# Assemble and checksum a Linux Cua Driver build.
#
#   pack_linux_artifact.sh --version 0.46.1 --arch x86_64 \
#       --target x86_64-unknown-linux-gnu --out-dir dist
#
# Mirrors the packaging half of the upstream `build-linux` job in
# .github/workflows/cd-rust-cua-driver.yml, so this fork's archive matches the
# layout the upstream verifier and install.sh expect:
#
#   cua-driver-rs-<version>-linux-<arch>.tar.gz         full archive
#   cua-driver-rs-<version>-linux-<arch>-binary.tar.gz  CLI + UniFFI library
#   SHA256SUMS
#
# Signing is .github/scripts/linux-sbx/sign_release_assets.sh, run after this.
set -euo pipefail

VERSION=""
ARCH=""
TARGET=""
OUT_DIR=""
RUST_DIR=""

usage() {
    cat <<'EOF'
usage: pack_linux_artifact.sh --version X.Y.Z --arch ARCH --target TRIPLE [options]

  --version X.Y.Z   Version recorded in the archive names (required).
  --arch ARCH       Architecture label, e.g. x86_64 (required).
  --target TRIPLE   Cargo target triple whose release/ output is packaged (required).
  --rust-dir DIR    libs/cua-driver/rust (default: the checkout containing this script).
  --out-dir DIR     Where assets are written (default: <rust-dir>/release/dist).
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version) VERSION="$2"; shift 2 ;;
        --arch) ARCH="$2"; shift 2 ;;
        --target) TARGET="$2"; shift 2 ;;
        --rust-dir) RUST_DIR="$2"; shift 2 ;;
        --out-dir) OUT_DIR="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

if [[ -z $VERSION || -z $ARCH || -z $TARGET ]]; then
    echo "error: --version, --arch and --target are required" >&2
    exit 2
fi

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEFAULT_ROOT="$(cd "$HERE/../../../.." && pwd)"
RUST_DIR="${RUST_DIR:-$DEFAULT_ROOT/libs/cua-driver/rust}"
command -v realpath >/dev/null 2>&1 && RUST_DIR="$(realpath "$RUST_DIR")"
# The repository root is derived from the rust directory actually packaged, not
# from this script's own location, so --rust-dir pointing at another checkout
# still finds that checkout's LICENSE.
REPO_ROOT="$(cd "$RUST_DIR/../../.." && pwd)"
OUT_DIR="${OUT_DIR:-$RUST_DIR/release/dist}"

RELEASE_DIR="$RUST_DIR/target/$TARGET/release"
STAGE_NAME="cua-driver-rs-${VERSION}-linux-${ARCH}"
STAGE="$RUST_DIR/release/$STAGE_NAME"

for member in cua-driver cua-cursor-theme libcua_driver_sdk.so cua_driver_node_runtime.node; do
    [[ -f "$RELEASE_DIR/$member" ]] || { echo "error: missing $RELEASE_DIR/$member; run the build first" >&2; exit 1; }
done
for member in "$RUST_DIR/include/cua_driver_abi.h" "$RUST_DIR/wayland-helper" \
              "$RUST_DIR/THIRD_PARTY_NOTICES.md" "$REPO_ROOT/LICENSE.md"; do
    [[ -e $member ]] || { echo "error: missing $member" >&2; exit 1; }
done

rm -rf "$STAGE"
mkdir -p "$STAGE" "$OUT_DIR"
cp "$RELEASE_DIR/cua-driver" "$RELEASE_DIR/cua-cursor-theme" \
   "$RELEASE_DIR/libcua_driver_sdk.so" "$RELEASE_DIR/cua_driver_node_runtime.node" "$STAGE/"
cp "$RUST_DIR/include/cua_driver_abi.h" "$STAGE/"
cp -R "$RUST_DIR/wayland-helper" "$STAGE/wayland-helper"
cp "$REPO_ROOT/LICENSE.md" "$STAGE/LICENSE"
cp "$RUST_DIR/THIRD_PARTY_NOTICES.md" "$STAGE/THIRD_PARTY_NOTICES.md"

(cd "$RUST_DIR/release" && tar -czf "$OUT_DIR/${STAGE_NAME}.tar.gz" "$STAGE_NAME")
tar -czf "$OUT_DIR/${STAGE_NAME}-binary.tar.gz" -C "$STAGE" \
    cua-driver cua-cursor-theme libcua_driver_sdk.so cua_driver_node_runtime.node cua_driver_abi.h wayland-helper

if command -v sha256sum >/dev/null 2>&1; then
    SHA_CMD=(sha256sum)
elif command -v shasum >/dev/null 2>&1; then
    # macOS. Both tools print "<hash>  <name>", so SHA256SUMS is byte-identical
    # to the file the released archives ship.
    SHA_CMD=(shasum -a 256)
else
    echo "error: need sha256sum or shasum to checksum the archives" >&2
    exit 1
fi

(
    cd "$OUT_DIR"
    "${SHA_CMD[@]}" "${STAGE_NAME}.tar.gz" "${STAGE_NAME}-binary.tar.gz" | sort -k2 >SHA256SUMS
    cat SHA256SUMS
)

ls -lh "$OUT_DIR"

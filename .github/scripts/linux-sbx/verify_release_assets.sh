#!/usr/bin/env bash
# Check that a published prerelease carries everything an installer verifies.
#
#   verify_release_assets.sh --tag cua-driver-rs-v0.41.3-linux-sbx.1 [--arch x86_64]
#
# Required on the release, read back with `gh release view <tag>`:
#   - both archives: cua-driver-rs-<ver>-linux-<arch>.tar.gz and -binary.tar.gz
#   - SHA256SUMS
#   - for every one of those files: .sigstore.json, .sig and .pem
#
# A tag is always named: this never asks GitHub for a "latest" release.
# Exit 0 when complete, 1 listing every missing file otherwise.
#
# `gh` comes from PATH and authenticates with GH_TOKEN / GH_REPO.
set -euo pipefail

TAG=""
ARCH="x86_64"
SUMMARY="${GITHUB_STEP_SUMMARY:-}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --tag) TAG="$2"; shift 2 ;;
        --arch) ARCH="$2"; shift 2 ;;
        --summary) SUMMARY="$2"; shift 2 ;;
        -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

if [[ ! $TAG =~ ^cua-driver-rs-v([0-9]+\.[0-9]+\.[0-9]+-linux-sbx\.[0-9]+)$ ]]; then
    echo "error: --tag must look like cua-driver-rs-vX.Y.Z-linux-sbx.N (got '$TAG')" >&2
    exit 2
fi
VERSION="${BASH_REMATCH[1]}"

if ! ASSETS="$(gh release view "$TAG" --json assets --jq '.assets[].name')"; then
    echo "::error::cannot read release $TAG"
    exit 1
fi
printf '%s\n' "$ASSETS"

STAGE="cua-driver-rs-${VERSION}-linux-${ARCH}"
SIGNED=("${STAGE}.tar.gz" "${STAGE}-binary.tar.gz" SHA256SUMS)

MISSING=()
for asset in "${SIGNED[@]}"; do
    for name in "$asset" "$asset.sigstore.json" "$asset.sig" "$asset.pem"; do
        grep -qxF "$name" <<<"$ASSETS" || MISSING+=("$name")
    done
done

if [[ ${#MISSING[@]} -gt 0 ]]; then
    echo "::error::release $TAG is missing ${MISSING[*]}"
    exit 1
fi

echo "release $TAG carries ${#SIGNED[@]} signed files"
if [[ -n $SUMMARY ]]; then
    echo "- verified ${#SIGNED[@]} assets and their signatures on \`$TAG\`" >>"$SUMMARY"
fi

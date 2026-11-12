#!/usr/bin/env bash
# Materialise the rebased source from the rebase job's bundle (build job).
#
#   checkout_from_bundle.sh --source-dir DIR --upstream-url URL [--dest DIR] [--trust-dest]
#                           [--copy-metadata-to DIR]
#
# DIR holds linux-sbx-source.bundle and linux-sbx-metadata.txt. The fork's
# remote is never read: the commit built is the one in the bundle.
#
#   1. validate the metadata, check the bundle's sha256 against it;
#   2. fetch the upstream tag commit (the bundle's prerequisite), depth 1;
#   3. `git bundle verify`, then fetch the branch out of the bundle;
#   4. check out that commit and confirm it is metadata's rebased_sha.
#
# --trust-dest marks DEST as a safe.directory in the global git config: in a
# container the workspace is owned by another uid and git would otherwise
# refuse it. Only the build container passes it.
#
# --copy-metadata-to DIR copies the metadata that was checked into DIR, so the
# build artifact carries exactly what the build was made from.
set -euo pipefail

SRC=""
UPSTREAM_URL=""
DEST="."
TRUST=0
COPY_META=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --source-dir) SRC="$2"; shift 2 ;;
        --upstream-url) UPSTREAM_URL="$2"; shift 2 ;;
        --dest) DEST="$2"; shift 2 ;;
        --trust-dest) TRUST=1; shift ;;
        --copy-metadata-to) COPY_META="$2"; shift 2 ;;
        -h|--help) sed -n '2,21p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
[[ -n $SRC && -n $UPSTREAM_URL ]] || { echo "error: --source-dir and --upstream-url are required" >&2; exit 2; }

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
META="$SRC/linux-sbx-metadata.txt"
m() { bash "$HERE/metadata.sh" "$META" "$1"; }

bash "$HERE/metadata.sh" "$META" --check
BUNDLE="$SRC/$(m bundle)"
WANT_SHA256="$(m bundle_sha256)"
TAG="$(m upstream_tag)"
TAG_SHA="$(m upstream_sha)"
BRANCH="$(m branch)"
REBASED="$(m rebased_sha)"

if command -v sha256sum >/dev/null 2>&1; then SHA_CMD=(sha256sum); else SHA_CMD=(shasum -a 256); fi
GOT_SHA256="$("${SHA_CMD[@]}" "$BUNDLE" | awk '{print $1}')"
if [[ $GOT_SHA256 != "$WANT_SHA256" ]]; then
    echo "::error::bundle sha256 $GOT_SHA256 does not match metadata $WANT_SHA256"
    exit 1
fi

mkdir -p "$DEST"
DEST="$(cd "$DEST" && pwd)"
if [[ $TRUST -eq 1 ]]; then
    git config --global --add safe.directory "$DEST"
fi
cd "$DEST"
[[ -d .git ]] || git init --quiet .

git fetch --quiet --depth=1 --no-tags "$UPSTREAM_URL" "+refs/tags/${TAG}:refs/tags/${TAG}"
[[ "$(git rev-parse "${TAG}^{commit}")" == "$TAG_SHA" ]] \
    || { echo "::error::upstream $TAG is not $TAG_SHA any more"; exit 1; }

git bundle verify "$BUNDLE" >/dev/null 2>&1 || { echo "::error::git bundle verify failed for $BUNDLE"; exit 1; }
git fetch --quiet "$BUNDLE" "+refs/heads/${BRANCH}:refs/heads/${BRANCH}"
git checkout --quiet --force "$BRANCH"

GOT="$(git rev-parse HEAD)"
if [[ $GOT != "$REBASED" ]]; then
    echo "::error::bundle branch is at $GOT, metadata says $REBASED"
    exit 1
fi
echo "checked out $BRANCH at $GOT (on $TAG) from $(basename "$BUNDLE")"
if [[ -n $COPY_META ]]; then
    mkdir -p "$COPY_META"
    cp "$META" "$COPY_META/"
fi

#!/usr/bin/env bash
# Package the rebased branch as a git bundle plus its metadata, for the build
# job and for publish_local.sh. Nothing is pushed anywhere.
#
#   make_source_bundle.sh --branch linux-sbx-patch --upstream-tag cua-driver-rs-v0.41.3 \
#       --previous-sha <branch sha before the rebase> \
#       --prerelease-tag cua-driver-rs-v0.41.3-linux-sbx.2 --out DIR \
#       [--rebase-result rebased] [--patch-commits 3] [--run-id 123] [--repository owner/repo]
#
# The bundle holds refs/heads/<branch> and only the commits above the upstream
# tag (its single prerequisite), so it stays small; a reader needs that tag
# commit first (upstream is public). Written to DIR:
#
#   linux-sbx-source.bundle
#   linux-sbx-metadata.txt     key=value lines, one per field below
#
#   upstream_tag  upstream_sha  branch  rebased_sha  previous_sha
#   bundle  bundle_sha256  prerelease_tag  prerelease_version
#   rebase_result  patch_commits  run_id  repository
#
# The metadata is data, never sourced: read it with a strict key=value parser
# (publish_local.sh and checkout_from_bundle.sh do).
set -euo pipefail

BRANCH=""
UPSTREAM_TAG=""
PREVIOUS_SHA=""
PRERELEASE_TAG=""
OUT=""
REBASE_RESULT=""
PATCH_COMMITS=""
RUN_ID=""
REPOSITORY=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --branch) BRANCH="$2"; shift 2 ;;
        --upstream-tag) UPSTREAM_TAG="$2"; shift 2 ;;
        --previous-sha) PREVIOUS_SHA="$2"; shift 2 ;;
        --prerelease-tag) PRERELEASE_TAG="$2"; shift 2 ;;
        --out) OUT="$2"; shift 2 ;;
        --rebase-result) REBASE_RESULT="$2"; shift 2 ;;
        --patch-commits) PATCH_COMMITS="$2"; shift 2 ;;
        --run-id) RUN_ID="$2"; shift 2 ;;
        --repository) REPOSITORY="$2"; shift 2 ;;
        -h|--help) sed -n '2,23p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

if [[ -z $BRANCH || -z $UPSTREAM_TAG || -z $PREVIOUS_SHA || -z $PRERELEASE_TAG || -z $OUT ]]; then
    echo "error: --branch, --upstream-tag, --previous-sha, --prerelease-tag and --out are required" >&2
    exit 2
fi
[[ $UPSTREAM_TAG =~ ^cua-driver-rs-v([0-9]+\.[0-9]+\.[0-9]+)$ ]] \
    || { echo "error: --upstream-tag must be cua-driver-rs-vX.Y.Z (got '$UPSTREAM_TAG')" >&2; exit 2; }
[[ $PRERELEASE_TAG =~ ^cua-driver-rs-v([0-9]+\.[0-9]+\.[0-9]+-linux-sbx\.[0-9]+)$ ]] \
    || { echo "error: --prerelease-tag must be cua-driver-rs-vX.Y.Z-linux-sbx.N (got '$PRERELEASE_TAG')" >&2; exit 2; }
PRERELEASE_VERSION="${BASH_REMATCH[1]}"
[[ $PREVIOUS_SHA =~ ^[0-9a-f]{40}$ ]] \
    || { echo "error: --previous-sha must be a full sha (got '$PREVIOUS_SHA')" >&2; exit 2; }

UPSTREAM_SHA="$(git rev-parse --verify --quiet "${UPSTREAM_TAG}^{commit}")" \
    || { echo "error: unknown upstream tag $UPSTREAM_TAG" >&2; exit 1; }
REBASED_SHA="$(git rev-parse --verify --quiet "refs/heads/${BRANCH}")" \
    || { echo "error: unknown branch $BRANCH" >&2; exit 1; }
git merge-base --is-ancestor "$UPSTREAM_SHA" "$REBASED_SHA" \
    || { echo "error: $BRANCH ($REBASED_SHA) is not based on $UPSTREAM_TAG" >&2; exit 1; }

if command -v sha256sum >/dev/null 2>&1; then SHA_CMD=(sha256sum); else SHA_CMD=(shasum -a 256); fi

mkdir -p "$OUT"
BUNDLE_NAME="linux-sbx-source.bundle"
BUNDLE="$OUT/$BUNDLE_NAME"
rm -f "$BUNDLE"
# `^<tag sha>` makes the tag commit the prerequisite instead of shipping the
# whole upstream history.
git bundle create "$BUNDLE" "refs/heads/${BRANCH}" "^${UPSTREAM_SHA}" 2>/dev/null \
    || { echo "error: git bundle create failed (is $BRANCH above $UPSTREAM_TAG?)" >&2; exit 1; }
git bundle verify "$BUNDLE" >/dev/null 2>&1 \
    || { echo "error: the bundle just written does not verify" >&2; exit 1; }
BUNDLE_SHA256="$("${SHA_CMD[@]}" "$BUNDLE" | awk '{print $1}')"

{
    echo "upstream_tag=$UPSTREAM_TAG"
    echo "upstream_sha=$UPSTREAM_SHA"
    echo "branch=$BRANCH"
    echo "rebased_sha=$REBASED_SHA"
    echo "previous_sha=$PREVIOUS_SHA"
    echo "bundle=$BUNDLE_NAME"
    echo "bundle_sha256=$BUNDLE_SHA256"
    echo "prerelease_tag=$PRERELEASE_TAG"
    echo "prerelease_version=$PRERELEASE_VERSION"
    echo "rebase_result=${REBASE_RESULT:-unknown}"
    echo "patch_commits=${PATCH_COMMITS:-0}"
    echo "run_id=${RUN_ID:-local}"
    echo "repository=${REPOSITORY:-unknown}"
} >"$OUT/linux-sbx-metadata.txt"

cat "$OUT/linux-sbx-metadata.txt"

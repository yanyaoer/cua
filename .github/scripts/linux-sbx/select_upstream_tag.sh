#!/usr/bin/env bash
# Pick the newest stable Cua Driver release tag from the upstream repository.
#
#   select_upstream_tag.sh --remote-url https://github.com/trycua/cua
#   select_upstream_tag.sh --remote-url URL --exact cua-driver-rs-v0.46.1
#
# Every Cua Driver release is tagged cua-driver-rs-v<major>.<minor>.<patch> by
# Release Please. This lists those tags with `git ls-remote` (no clone and no
# token: the upstream repository is public) and prints the highest one by
# numeric major, minor and patch. Tags carrying a pre-release or build
# metadata suffix (`-beta.1`, `+linux-sbx.2`) and tags of other components are
# ignored, so a nightly or this fork's own prerelease can never become the
# weekly base.
#
# The repository-wide "Latest" release is never consulted: this fork publishes
# prereleases of its own and GitHub's badge is cosmetic.
#
# Outputs, written to --output (default $GITHUB_OUTPUT) when that is set:
#   tag=<cua-driver-rs-vX.Y.Z>   version=<X.Y.Z>
# The tag is also printed on stdout for an interactive run.
set -euo pipefail

TAG_PREFIX="cua-driver-rs-v"
STABLE_RE="^${TAG_PREFIX}[0-9]+\.[0-9]+\.[0-9]+$"

REMOTE_URL=""
EXACT=""
OUTPUT="${GITHUB_OUTPUT:-}"

usage() {
    cat <<'EOF'
usage: select_upstream_tag.sh --remote-url URL [--exact TAG] [--output FILE]

  --remote-url URL   Git URL of the upstream repository (required).
  --exact TAG        Use this tag instead of the highest one; it must exist
                     upstream and be a stable cua-driver-rs-vX.Y.Z tag.
  --output FILE      Write tag= and version= here (default: $GITHUB_OUTPUT).
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --remote-url) REMOTE_URL="$2"; shift 2 ;;
        --exact) EXACT="$2"; shift 2 ;;
        --output) OUTPUT="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

if [[ -z $REMOTE_URL ]]; then
    echo "error: --remote-url is required" >&2
    exit 2
fi

if [[ -n $EXACT && ! $EXACT =~ $STABLE_RE ]]; then
    echo "error: --exact must look like ${TAG_PREFIX}X.Y.Z (got '$EXACT')" >&2
    exit 2
fi

if ! RAW="$(git ls-remote --tags --refs "$REMOTE_URL" "refs/tags/${TAG_PREFIX}*")"; then
    echo "error: cannot list tags from $REMOTE_URL" >&2
    exit 1
fi

TAGS="$(printf '%s\n' "$RAW" | awk 'NF { sub("^refs/tags/", "", $2); print $2 }' | grep -E "$STABLE_RE" || true)"
if [[ -z $TAGS ]]; then
    echo "error: no stable ${TAG_PREFIX}X.Y.Z tag at $REMOTE_URL" >&2
    exit 1
fi

BEST=""
BEST_KEY=""
while IFS= read -r tag; do
    [[ -n $tag ]] || continue
    version="${tag#"$TAG_PREFIX"}"
    IFS=. read -r major minor patch <<<"$version"
    # Zero padded so a plain string comparison orders by number, not by the
    # width of each component (0.9.0 sorts below 0.10.0).
    key="$(printf '%08d%08d%08d' "$major" "$minor" "$patch")"
    if [[ -z $BEST_KEY || $key > $BEST_KEY ]]; then
        BEST="$tag"
        BEST_KEY="$key"
    fi
done <<<"$TAGS"

if [[ -n $EXACT ]]; then
    if ! grep -qxF "$EXACT" <<<"$TAGS"; then
        echo "error: $EXACT is not a stable ${TAG_PREFIX}X.Y.Z tag at $REMOTE_URL" >&2
        echo "stable tags available: $(tr '\n' ' ' <<<"$TAGS")" >&2
        exit 1
    fi
    BEST="$EXACT"
fi

VERSION="${BEST#"$TAG_PREFIX"}"

if [[ -n $OUTPUT ]]; then
    {
        echo "tag=$BEST"
        echo "version=$VERSION"
    } >>"$OUTPUT"
fi

echo "selected $BEST (highest stable ${TAG_PREFIX}X.Y.Z tag upstream)" >&2
echo "$BEST"

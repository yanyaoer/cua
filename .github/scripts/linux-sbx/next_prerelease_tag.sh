#!/usr/bin/env bash
# Compute this fork's next prerelease tag for a Cua Driver version.
#
#   next_prerelease_tag.sh --version 0.46.1 --remote-url https://github.com/<owner>/cua
#
# The fork publishes one prerelease per weekly build, tagged
#
#   cua-driver-rs-v<upstream version>-linux-sbx.<N>
#
# with N starting at 1 and incrementing per version. Existing tags are listed
# with `git ls-remote` so a re-run after a partial failure never reuses a tag:
# the next number is always one past the highest already pushed. The suffix
# carries no product name beyond the branch it is built from.
#
# Outputs, written to --output (default $GITHUB_OUTPUT) when set:
#   tag=<cua-driver-rs-vX.Y.Z-linux-sbx.N>
#   prerelease=<X.Y.Z-linux-sbx.N>
#   next=<N>
set -euo pipefail

VERSION=""
REMOTE_URL=""
OUTPUT="${GITHUB_OUTPUT:-}"
SUFFIX="linux-sbx"

usage() {
    cat <<'EOF'
usage: next_prerelease_tag.sh --version X.Y.Z --remote-url URL [--output FILE]

  --version X.Y.Z    Upstream Cua Driver version the build came from (required).
  --remote-url URL   Repository whose existing prerelease tags are counted
                     (required; tags only, no token needed for a public repo).
  --suffix NAME      Prerelease suffix (default: linux-sbx).
  --output FILE      Write tag=, prerelease=, next= here (default: $GITHUB_OUTPUT).
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version) VERSION="$2"; shift 2 ;;
        --remote-url) REMOTE_URL="$2"; shift 2 ;;
        --suffix) SUFFIX="$2"; shift 2 ;;
        --output) OUTPUT="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

if [[ -z $VERSION || -z $REMOTE_URL ]]; then
    echo "error: --version and --remote-url are required" >&2
    exit 2
fi

if [[ ! $VERSION =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "error: --version must be X.Y.Z without a prerelease suffix (got '$VERSION')" >&2
    exit 2
fi

BASE="cua-driver-rs-v${VERSION}-${SUFFIX}"
NUM_RE="^cua-driver-rs-v$(
    printf '%s' "$VERSION" | sed 's/\./\\./g'
)-${SUFFIX//./\\.}\.([0-9]+)$"

if ! RAW="$(git ls-remote --tags --refs "$REMOTE_URL" "refs/tags/${BASE}.*")"; then
    echo "error: cannot list tags from $REMOTE_URL" >&2
    exit 1
fi

HIGHEST=0
while IFS= read -r tag; do
    [[ -n $tag ]] || continue
    if [[ $tag =~ $NUM_RE ]]; then
        n="${BASH_REMATCH[1]}"
        # 10# forces base 10: bash would read 08 and 09 as invalid octal.
        n=$((10#$n))
        if ((n > HIGHEST)); then HIGHEST=$n; fi
    fi
done < <(printf '%s\n' "$RAW" | awk 'NF { sub("^refs/tags/", "", $2); print $2 }')

NEXT=$((HIGHEST + 1))
TAG="${BASE}.${NEXT}"

if [[ -n $OUTPUT ]]; then
    {
        echo "tag=$TAG"
        echo "prerelease=${VERSION}-${SUFFIX}.${NEXT}"
        echo "next=$NEXT"
    } >>"$OUTPUT"
fi

echo "$TAG"

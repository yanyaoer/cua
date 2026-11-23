#!/usr/bin/env bash
# Try the weekly rebase early: replay the patch commits onto upstream's main.
#
#   rebase_probe.sh --dir DIR --repo URL [--branch linux-sbx]
#                   --upstream-url URL [--upstream-branch main]
#                   [--conflict-report FILE] [--output FILE]
#
# The weekly build rebases onto the newest release tag, and a conflict there
# stops that week's release. Upstream tags often, from main: replaying the same
# patch commits onto main's head shows a conflict days before a tag carries it,
# while there is still time to resolve it by hand. This only tries: nothing is
# pushed, built or published, and the result says nothing about whether the
# rebased tree compiles or passes the bridge tests (the weekly build checks
# that).
#
# DIR is a clone kept between runs (created from --repo when missing), so a
# run fetches only what changed. Its `linux-sbx-patch` branch is reset to the
# fork's --branch every run.
#
# Prints and writes to --output: result=clean|conflict, upstream_sha=,
# branch_sha=, and on a conflict stopped_commit= and files= (space separated).
# Exit 0 clean, 10 conflict (report in --conflict-report), 1 anything else.
set -euo pipefail

DIR=""; REPO=""; BRANCH="linux-sbx"; UPSTREAM_URL=""; UPSTREAM_BRANCH="main"
REPORT=""; OUTPUT=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --dir) DIR="$2"; shift 2 ;;
        --repo) REPO="$2"; shift 2 ;;
        --branch) BRANCH="$2"; shift 2 ;;
        --upstream-url) UPSTREAM_URL="$2"; shift 2 ;;
        --upstream-branch) UPSTREAM_BRANCH="$2"; shift 2 ;;
        --conflict-report) REPORT="$2"; shift 2 ;;
        --output) OUTPUT="$2"; shift 2 ;;
        -h|--help) sed -n '2,21p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
[[ -n $DIR && -n $REPO && -n $UPSTREAM_URL ]] \
    || { echo "error: --dir, --repo and --upstream-url are required" >&2; exit 2; }

retry() { # github.com drops some connections from build hosts
    local i
    for i in 1 2 3 4; do
        "$@" && return 0
        [[ $i -lt 4 ]] && sleep $((i * 15))
    done
    return 1
}
emit() {
    printf '%s\n' "$@"
    [[ -z $OUTPUT ]] || printf '%s\n' "$@" >>"$OUTPUT"
}
[[ -z $OUTPUT ]] || : >"$OUTPUT"

# The rebase rewrites the work tree it runs in, scripts included: run a copy.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TOOLS="$(mktemp -d)"
trap 'rm -rf "$TOOLS"' EXIT
cp "$HERE/rebase_onto_tag.sh" "$TOOLS/"
REPORT="${REPORT:-$TOOLS/conflict.md}"

if [[ ! -d $DIR/.git ]]; then
    retry git -c http.version=HTTP/1.1 clone --quiet --no-checkout "$REPO" "$DIR"
fi
cd "$DIR"
git config http.version HTTP/1.1
# Replayed commits need a committer; never this host's identity.
git config user.name "linux-sbx probe"
git config user.email "linux-sbx-probe@localhost"
if [[ -d "$(git rev-parse --git-path rebase-merge)" || -d "$(git rev-parse --git-path rebase-apply)" ]]; then
    git rebase --abort >/dev/null 2>&1 || true
fi
retry git fetch --quiet --force "$REPO" "+refs/heads/$BRANCH:refs/remotes/fork/$BRANCH"
retry git fetch --quiet --force --no-tags "$UPSTREAM_URL" \
    "+refs/heads/$UPSTREAM_BRANCH:refs/remotes/upstream/$UPSTREAM_BRANCH" \
    "+refs/tags/cua-driver-rs-v*:refs/tags/cua-driver-rs-v*"
BRANCH_SHA="$(git rev-parse "refs/remotes/fork/$BRANCH")"
UPSTREAM_SHA="$(git rev-parse "refs/remotes/upstream/$UPSTREAM_BRANCH")"
git checkout --quiet --force -B linux-sbx-patch "$BRANCH_SHA"

set +e
bash "$TOOLS/rebase_onto_tag.sh" --branch linux-sbx-patch --upstream-ref "$UPSTREAM_SHA" \
    --output "$TOOLS/rebase.env" --conflict-report "$REPORT" >"$TOOLS/rebase.log" 2>&1
rc=$?
set -e
emit "upstream_sha=$UPSTREAM_SHA" "branch_sha=$BRANCH_SHA"
case $rc in
    0)
        emit "result=clean"
        ;;
    10)
        # The report names the stopped commit and the files; keep them short
        # and stable, so a caller can tell this conflict from the last one.
        STOPPED="$(sed -n 's/^- stopped at: `\([0-9a-f]*\)`.*/\1/p' "$REPORT" | head -1)"
        FILES="$(sed -n 's/^- `\(.*\)`$/\1/p' "$REPORT" | sort -u | tr '\n' ' ' | sed 's/ $//')"
        emit "result=conflict" "stopped_commit=$STOPPED" "files=$FILES"
        ;;
    *)
        cat "$TOOLS/rebase.log" >&2
        echo "error: rebase_onto_tag.sh exited $rc" >&2
        exit 1
        ;;
esac
git checkout --quiet --force --detach "$BRANCH_SHA"
exit "$rc"

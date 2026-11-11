#!/usr/bin/env bash
# Replay the fork's patch commits onto a Cua Driver release tag.
#
#   rebase_onto_tag.sh --branch linux-sbx-patch --upstream-ref cua-driver-rs-v0.46.1 \
#       --upstream-url https://github.com/trycua/cua --push
#
# The patch commits are the commits on --branch that are not reachable from any
# stable upstream release tag (cua-driver-rs-vX.Y.Z). Every other commit on the
# branch came from upstream, so it is replaced by the tag rather than replayed.
#
# Modes reported in $GITHUB_OUTPUT and the step summary:
#   result=up-to-date     the branch already points at the tag
#   result=fast-forward   no patch commits; the branch moves to the tag
#   result=rebased        patch commits replayed cleanly; <branch> = <new sha>
#   (no output)           conflict; exit code 10, branch untouched, nothing pushed
#
# A conflict never force-pushes and never leaves a rebase in progress: the
# conflicting files, the tag and a `git status` summary go to
# --conflict-report (for the artifact) and --summary (for the run page), the
# rebase is aborted, and the script exits 10. The caller decides how to shout
# about it; this script only refuses to continue.
#
# Pushing is opt-in (--push) and always a --force-with-lease bound to the
# remote ref observed before the rebase, so a branch that moved underneath us
# fails instead of being overwritten.
set -euo pipefail

BRANCH=""
UPSTREAM_REF=""
UPSTREAM_URL=""
REMOTE="origin"
PUSH=0
OUTPUT="${GITHUB_OUTPUT:-}"
SUMMARY="${GITHUB_STEP_SUMMARY:-}"
CONFLICT_REPORT=""

usage() {
    cat <<'EOF'
usage: rebase_onto_tag.sh --branch NAME --upstream-ref TAG [options]

  --branch NAME        Branch whose patch commits are replayed (required).
  --upstream-ref TAG   Upstream release tag to rebase onto (required).
  --upstream-url URL   Fetch that repository's cua-driver-rs-v* tags first.
  --remote NAME        Remote the branch is pushed to (default: origin).
  --push               Force-with-lease push the rebased branch after success.
  --output FILE        Write result=, branch=, sha=, previous_sha= (default: $GITHUB_OUTPUT).
  --summary FILE       Append a Markdown summary (default: $GITHUB_STEP_SUMMARY).
  --conflict-report FILE
                       Write the Markdown conflict report to FILE.

Exit codes: 0 success, 10 rebase conflict, 2 bad usage, 1 any other failure.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --branch) BRANCH="$2"; shift 2 ;;
        --upstream-ref) UPSTREAM_REF="$2"; shift 2 ;;
        --upstream-url) UPSTREAM_URL="$2"; shift 2 ;;
        --remote) REMOTE="$2"; shift 2 ;;
        --push) PUSH=1; shift ;;
        --output) OUTPUT="$2"; shift 2 ;;
        --summary) SUMMARY="$2"; shift 2 ;;
        --conflict-report) CONFLICT_REPORT="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

if [[ -z $BRANCH || -z $UPSTREAM_REF ]]; then
    echo "error: --branch and --upstream-ref are required" >&2
    exit 2
fi

die() {
    echo "error: $*" >&2
    exit 1
}

emit() {
    [[ -n $OUTPUT ]] || return 0
    printf '%s\n' "$@" >>"$OUTPUT"
}

say() {
    [[ -n $SUMMARY ]] || return 0
    printf '%s\n' "$@" >>"$SUMMARY"
}

in_rebase() {
    [[ -d "$(git rev-parse --git-path rebase-merge)" || -d "$(git rev-parse --git-path rebase-apply)" ]]
}

git rev-parse --git-dir >/dev/null 2>&1 || die "not inside a git checkout"

# A checkout that arrives with a rebase in progress is not something this script
# can reason about: the patch commit set would already be half applied.
if in_rebase; then
    git rebase --abort >/dev/null 2>&1 || true
    die "a rebase was already in progress; aborted it, re-run the job"
fi

if [[ -n $UPSTREAM_URL ]]; then
    echo "fetching ${UPSTREAM_REF} and every cua-driver-rs-v* tag from $UPSTREAM_URL" >&2
    git fetch --force --no-tags "$UPSTREAM_URL" \
        "+refs/tags/cua-driver-rs-v*:refs/tags/cua-driver-rs-v*" >/dev/null
fi

git rev-parse --verify --quiet "${UPSTREAM_REF}^{commit}" >/dev/null \
    || die "unknown upstream ref: $UPSTREAM_REF"
TAG_SHA="$(git rev-parse "${UPSTREAM_REF}^{commit}")"

git rev-parse --verify --quiet "refs/heads/${BRANCH}" >/dev/null \
    || die "unknown branch: $BRANCH"
PREVIOUS_SHA="$(git rev-parse "refs/heads/${BRANCH}")"

# Stable release tags define the upstream baseline. The fork's own prerelease
# tags carry a suffix (cua-driver-rs-vX.Y.Z-linux-sbx.N) and are deliberately
# not matched: excluding them would hide patch commits they point at.
STABLE_TAGS=()
while IFS= read -r tag; do
    [[ -n $tag ]] && STABLE_TAGS+=("$tag")
done < <(git tag -l 'cua-driver-rs-v*' | grep -E '^cua-driver-rs-v[0-9]+\.[0-9]+\.[0-9]+$' || true)

PATCH_COMMITS=()
if [[ ${#STABLE_TAGS[@]} -gt 0 ]]; then
    # One --not, then every tag. --not flips the meaning of each following
    # revision up to the next --not, so `--not a --not b` re-includes b and the
    # range silently grows to include upstream again.
    while IFS= read -r commit; do
        [[ -n $commit ]] && PATCH_COMMITS+=("$commit")
    done < <(git rev-list --reverse "$BRANCH" --not "${STABLE_TAGS[@]}")
fi

say "### Rebase \`${BRANCH}\` onto \`${UPSTREAM_REF}\`"
say ""
say "- upstream baseline tags considered: ${#STABLE_TAGS[@]}"
say "- patch commits to replay: ${#PATCH_COMMITS[@]}"

# Already there: nothing to replay and nothing to move.
if [[ $PREVIOUS_SHA == "$TAG_SHA" ]]; then
    echo "${BRANCH} already points at ${UPSTREAM_REF} (${TAG_SHA})" >&2
    say "- result: already at the tag, nothing to do"
    emit "result=up-to-date" "branch=${BRANCH}" "sha=${TAG_SHA}" "previous_sha=${PREVIOUS_SHA}"
    exit 0
fi

# Only a replay needs an identity, and a fresh GitHub runner has none: the
# upstream branch commits already carry their author, so setting a committer
# here keeps a fast-forward run from failing on a missing user.name.
if ! git config user.name >/dev/null 2>&1; then
    git config user.name "github-actions[bot]"
fi
if ! git config user.email >/dev/null 2>&1; then
    git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
fi

if [[ ${#PATCH_COMMITS[@]} -eq 0 ]]; then
    echo "no patch commits; fast-forwarding ${BRANCH} to ${UPSTREAM_REF}" >&2
    git checkout --quiet --detach "$TAG_SHA"
    git branch --force "$BRANCH" "$TAG_SHA"
    git checkout --quiet "$BRANCH"
    MODE="fast-forward"
else
    OLD_BASE="$(git rev-parse "${PATCH_COMMITS[0]}^")"
    # The replay is a single --onto range, so the patch commits must be one
    # contiguous run on top of their baseline. If upstream commits sit between
    # them, replaying the range would duplicate those commits; stop instead.
    RANGE_COUNT="$(git rev-list --count "${OLD_BASE}..${BRANCH}")"
    if [[ $RANGE_COUNT -ne ${#PATCH_COMMITS[@]} ]]; then
        die "${BRANCH} interleaves patch and upstream commits (${RANGE_COUNT} commits above ${OLD_BASE}, ${#PATCH_COMMITS[@]} not in any release tag); rebase it by hand"
    fi
    echo "replaying ${#PATCH_COMMITS[@]} patch commit(s) onto ${UPSTREAM_REF}" >&2
    if git rebase --onto "$TAG_SHA" "$OLD_BASE" "$BRANCH" >/dev/null 2>&1; then
        MODE="rebased"
    else
        CONFLICTING=()
        while IFS= read -r path; do
            [[ -n $path ]] && CONFLICTING+=("$path")
        done < <(git diff --name-only --diff-filter=U || true)
        STATUS_SUMMARY="$(git status --short --branch 2>/dev/null || true)"
        # REBASE_HEAD is the commit whose patch failed to apply. HEAD is still
        # the last commit that applied (the new upstream base), which would name
        # the wrong change in the report.
        STOPPED_SHA="$(git rev-parse --verify --quiet REBASE_HEAD 2>/dev/null || git rev-parse HEAD 2>/dev/null || true)"
        CURRENT_COMMIT="$(git rev-parse --short "$STOPPED_SHA" 2>/dev/null || true)"
        CURRENT_SUBJECT="$(git log -1 --pretty=%s "$STOPPED_SHA" 2>/dev/null || true)"

        {
            echo "## Rebase conflict: \`${BRANCH}\` onto \`${UPSTREAM_REF}\`"
            echo ""
            echo "Nothing was pushed. The rebase was aborted, so both the local"
            echo "branch and the remote ref are unchanged."
            echo ""
            echo "- upstream tag: \`${UPSTREAM_REF}\` (${TAG_SHA})"
            echo "- branch: \`${BRANCH}\` (${PREVIOUS_SHA})"
            echo "- patch commits: ${#PATCH_COMMITS[@]}"
            if [[ -n $CURRENT_COMMIT ]]; then
                echo "- stopped at: \`${CURRENT_COMMIT}\` ${CURRENT_SUBJECT}"
            fi
            echo ""
            echo "### Conflicting files"
            echo ""
            if [[ ${#CONFLICTING[@]} -eq 0 ]]; then
                echo "git reported a failed rebase without unmerged paths; see the job log."
            else
                for path in "${CONFLICTING[@]}"; do echo "- \`${path}\`"; done
            fi
            echo ""
            echo "### git status"
            echo ""
            echo '```'
            echo "${STATUS_SUMMARY}"
            echo '```'
            echo ""
            echo "### Next step"
            echo ""
            echo "Resolve the conflict in a local checkout:"
            echo ""
            echo '```'
            echo "git fetch origin ${BRANCH}"
            echo "git switch ${BRANCH}"
            echo "git rebase ${UPSTREAM_REF}    # resolve, then git rebase --continue"
            echo "git push --force-with-lease origin ${BRANCH}"
            echo '```'
            echo ""
        } >"${CONFLICT_REPORT:-/dev/stdout}"

        git rebase --abort >/dev/null 2>&1 || true

        say ""
        say "Conflicts on ${#CONFLICTING[@]} file(s) while replaying onto \`${UPSTREAM_REF}\`; nothing pushed."
        if [[ ${#CONFLICTING[@]} -gt 0 ]]; then
            for path in "${CONFLICTING[@]}"; do say "- \`${path}\`"; done
        fi

        echo "error: rebase of ${BRANCH} onto ${UPSTREAM_REF} conflicted; nothing pushed" >&2
        if [[ ${#CONFLICTING[@]} -gt 0 ]]; then
            printf '  conflicting file: %s\n' "${CONFLICTING[@]}" >&2
        fi
        exit 10
    fi
fi

NEW_SHA="$(git rev-parse "refs/heads/${BRANCH}")"
PATCH_COUNT="${#PATCH_COMMITS[@]}"
say "- result: ${MODE}, \`${BRANCH}\` now at \`${NEW_SHA}\` (was \`${PREVIOUS_SHA}\`)"

if [[ $PUSH -eq 1 ]]; then
    # Guard against overwriting work pushed after this checkout fetched. The
    # expected value is what the fetch saw (refs/remotes/<remote>/<branch>), not
    # the local branch: local-only commits are normal outside CI and are not a
    # reason to refuse. A remote ref that differs from the fetched one means
    # someone else pushed, so stop instead of clobbering it.
    FETCHED_SHA="$(git rev-parse --verify --quiet "refs/remotes/${REMOTE}/${BRANCH}" || true)"
    REMOTE_SHA="$(git ls-remote "$REMOTE" "refs/heads/${BRANCH}" | awk 'NR==1 {print $1}')"
    if [[ -n $FETCHED_SHA && -n $REMOTE_SHA && $REMOTE_SHA != "$FETCHED_SHA" ]]; then
        die "remote ${REMOTE} refs/heads/${BRANCH} moved since it was fetched (${REMOTE_SHA}, fetched ${FETCHED_SHA}); not pushing, re-run to pick it up"
    fi
    EXPECTED_SHA="${FETCHED_SHA:-$REMOTE_SHA}"
    LEASE="refs/heads/${BRANCH}:${EXPECTED_SHA:-0000000000000000000000000000000000000000}"
    echo "pushing ${BRANCH} to ${REMOTE} (force-with-lease ${LEASE})" >&2
    git push --force-with-lease="$LEASE" "$REMOTE" "HEAD:refs/heads/${BRANCH}" >/dev/null
    say "- pushed: \`${REMOTE}/refs/heads/${BRANCH}\` at \`${NEW_SHA}\` (force-with-lease)"
else
    say "- push: skipped (--push not given)"
fi

emit "result=${MODE}" "branch=${BRANCH}" "sha=${NEW_SHA}" "previous_sha=${PREVIOUS_SHA}" "patch_commits=${PATCH_COUNT}"
echo "${MODE}: ${BRANCH} is now ${NEW_SHA} (${PATCH_COUNT} patch commit(s) on ${UPSTREAM_REF})" >&2

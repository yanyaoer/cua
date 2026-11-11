#!/usr/bin/env bash
# Turn a failed rebase into an issue in this repository, and always fail.
#
#   report_conflict.sh --report FILE --branch NAME [--tag TAG] [--run-url URL]
#
# Called after rebase_onto_tag.sh failed. Two cases:
#
#   - The report file exists: the rebase conflicted. Open an issue whose title
#     names the branch and the upstream tag and whose body is the report plus a
#     link to the run. If the issue cannot be created (Issues disabled on the
#     fork, no `issues: write`), print an error that points at the artifact.
#   - No report: the job failed for another reason (no tag upstream, a push
#     race, a missing branch). Say so rather than filing a misleading conflict.
#
# The exit status is 1 in every case. A conflict is a failure, and opening the
# issue is a notification, not a resolution: the run must end red either way.
#
# `gh` comes from PATH and authenticates with GH_TOKEN / GH_REPO.
set -euo pipefail

REPORT=""
SUMMARY="${GITHUB_STEP_SUMMARY:-}"
BRANCH=""
TAG=""
RUN_URL=""

usage() {
    cat <<'EOF'
usage: report_conflict.sh --report FILE --branch NAME [--tag TAG] [--run-url URL] [--summary FILE]

  --summary FILE   Also append the full report here (default: $GITHUB_STEP_SUMMARY).
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --report) REPORT="$2"; shift 2 ;;
        --branch) BRANCH="$2"; shift 2 ;;
        --tag) TAG="$2"; shift 2 ;;
        --run-url) RUN_URL="$2"; shift 2 ;;
        --summary) SUMMARY="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

if [[ -z $REPORT || -z $BRANCH ]]; then
    echo "error: --report and --branch are required" >&2
    exit 2
fi

WHERE="${RUN_URL:-the workflow run}"

if [[ ! -f $REPORT ]]; then
    echo "::error::the rebase job failed without a conflict report, so this is not a merge conflict; see $WHERE"
    exit 1
fi

# The whole report, git status included, on the run page as well as in the
# artifact and the issue.
if [[ -n $SUMMARY ]]; then
    cat "$REPORT" >>"$SUMMARY"
fi

TITLE="Rebase conflict: ${BRANCH} onto ${TAG:-an upstream tag}"
BODY="$(mktemp)"
trap 'rm -f "$BODY"' EXIT
{
    cat "$REPORT"
    echo ""
    echo "---"
    echo ""
    echo "Workflow run: $WHERE"
} >"$BODY"

if ! gh issue create --title "$TITLE" --body-file "$BODY"; then
    echo "::error::could not open the conflict issue '$TITLE'. Issues may be disabled on this fork, or the job token lacks issues: write. The conflict report is in the run artifacts ($WHERE)."
    exit 1
fi

echo "::error::rebase of ${BRANCH} onto ${TAG:-an upstream tag} conflicted; issue opened ('$TITLE'), nothing was pushed"
exit 1

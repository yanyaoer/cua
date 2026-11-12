#!/usr/bin/env bash
# Write the release notes and create this fork's prerelease.
#
#   publish_prerelease.sh --tag cua-driver-rs-v0.41.3-linux-sbx.1 \
#       --source-sha <sha> --upstream-tag cua-driver-rs-v0.41.3 \
#       --upstream-repo trycua/cua --repository owner/repo \
#       --branch linux-sbx-patch --dist dist \
#       [--rebase-result rebased] [--patch-commits 2] [--summary FILE]
#       [--existing-tag]
#
# The release is always:
#   - a prerelease, never the repository's Latest (--latest=false);
#   - tagged on --source-sha, the commit that was built, not the commit this
#     workflow happens to run from (--target);
#   - with --existing-tag: attached to the tag that is already on GitHub
#     (--verify-tag, no --target). The caller has checked that the tag points
#     at --source-sha; gh refuses if the tag is missing, and never moves it;
#   - carrying every file in --dist (archives, SHA256SUMS and their signatures).
#
# The notes are an introduction (source commit, rebase outcome, how to verify
# the signatures) followed by the body of upstream's own release for the tag,
# read publicly (`gh release view -R`): the generate-notes API would need write
# access to upstream. If that body cannot be read the release still goes out.
#
# `gh` comes from PATH and authenticates with GH_TOKEN / GH_REPO.
set -euo pipefail

TAG=""
SOURCE_SHA=""
UPSTREAM_TAG=""
UPSTREAM_REPO=""
REPOSITORY=""
BRANCH=""
DIST=""
REBASE_RESULT=""
PATCH_COMMITS=""
SUMMARY="${GITHUB_STEP_SUMMARY:-}"
EXISTING_TAG=0

usage() { sed -n '2,9p' "$0"; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --tag) TAG="$2"; shift 2 ;;
        --source-sha) SOURCE_SHA="$2"; shift 2 ;;
        --upstream-tag) UPSTREAM_TAG="$2"; shift 2 ;;
        --upstream-repo) UPSTREAM_REPO="$2"; shift 2 ;;
        --repository) REPOSITORY="$2"; shift 2 ;;
        --branch) BRANCH="$2"; shift 2 ;;
        --dist) DIST="$2"; shift 2 ;;
        --rebase-result) REBASE_RESULT="$2"; shift 2 ;;
        --patch-commits) PATCH_COMMITS="$2"; shift 2 ;;
        --summary) SUMMARY="$2"; shift 2 ;;
        --existing-tag) EXISTING_TAG=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

for pair in "--tag:$TAG" "--source-sha:$SOURCE_SHA" "--upstream-tag:$UPSTREAM_TAG" \
            "--upstream-repo:$UPSTREAM_REPO" "--repository:$REPOSITORY" \
            "--branch:$BRANCH" "--dist:$DIST"; do
    if [[ -z ${pair#*:} ]]; then
        echo "error: ${pair%%:*} is required" >&2
        exit 2
    fi
done

if [[ ! $TAG =~ ^cua-driver-rs-v[0-9]+\.[0-9]+\.[0-9]+-linux-sbx\.[0-9]+$ ]]; then
    echo "error: --tag must look like cua-driver-rs-vX.Y.Z-linux-sbx.N (got '$TAG')" >&2
    exit 2
fi
if [[ ! $SOURCE_SHA =~ ^[0-9a-f]{40}$ ]]; then
    echo "error: --source-sha must be a full commit sha (got '$SOURCE_SHA')" >&2
    exit 2
fi

shopt -s nullglob
ASSETS=("$DIST"/*)
shopt -u nullglob
if [[ ${#ASSETS[@]} -eq 0 ]]; then
    echo "error: nothing to publish in $DIST" >&2
    exit 1
fi

NOTES="$(mktemp)"
UPSTREAM_NOTES="$(mktemp)"
trap 'rm -f "$NOTES" "$UPSTREAM_NOTES"' EXIT

if ! gh release view "$UPSTREAM_TAG" --repo "$UPSTREAM_REPO" --json body --jq .body >"$UPSTREAM_NOTES"; then
    echo "::warning::could not fetch upstream notes for $UPSTREAM_TAG; publishing without them"
    : >"$UPSTREAM_NOTES"
fi

{
    echo "Linux x86_64 Cua Driver built from this fork's \`$BRANCH\`"
    echo "branch, rebased onto upstream \`$UPSTREAM_TAG\`."
    echo ""
    echo "- source commit: \`$SOURCE_SHA\`"
    echo "- rebase: ${REBASE_RESULT:-unknown}"
    echo "- patch commits: ${PATCH_COMMITS:-0}"
    echo ""
    echo "Every asset and \`SHA256SUMS\` carry a keyless Sigstore bundle"
    echo "(\`.sigstore.json\`), a detached signature (\`.sig\`) and the signing"
    echo "certificate (\`.pem\`). Verify with:"
    echo ""
    echo '```'
    echo "cosign verify-blob --bundle <asset>.sigstore.json \\"
    echo "  --certificate-identity-regexp 'https://github.com/${REPOSITORY}/' \\"
    echo "  --certificate-oidc-issuer https://token.actions.githubusercontent.com <asset>"
    echo '```'
    echo ""
    cat "$UPSTREAM_NOTES"
} >"$NOTES"

# --latest=false: a component prerelease never takes the repository-wide
# Latest badge.
if [[ $EXISTING_TAG -eq 1 ]]; then
    TAG_ARGS=(--verify-tag)
else
    TAG_ARGS=(--target "$SOURCE_SHA")
fi
gh release create "$TAG" \
    "${TAG_ARGS[@]}" \
    --title "$TAG" \
    --notes-file "$NOTES" \
    --prerelease \
    --latest=false \
    "${ASSETS[@]}"

if [[ -n $SUMMARY ]]; then
    {
        echo "### Published \`$TAG\`"
        echo ""
        echo "- https://github.com/${REPOSITORY}/releases/tag/$TAG"
    } >>"$SUMMARY"
fi
echo "published $TAG at $SOURCE_SHA with ${#ASSETS[@]} asset(s)"

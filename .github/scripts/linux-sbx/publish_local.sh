#!/usr/bin/env bash
# Publish one weekly run from this machine, with the local `gh` login.
#
#   publish_local.sh <run-id> [--dry-run] [--repo yanyaoer/cua] [--remote origin]
#                    [--branch linux-sbx-patch] [--upstream-url URL]
#                    [--upstream-repo trycua/cua] [--workdir DIR]
#                    [--assume-workflow-scope]
#
# Actions cannot push linux-sbx-patch (the job token may not update workflow
# files) and holds no other credential, so the weekly run stops at an artifact.
# This script finishes it. Run it from a checkout of the fork:
#
#   verify (read-only; any failure exits non-zero before anything is written)
#     0. the repository (--repo, or what `gh repo view` reports) is yanyaoer/cua,
#        the only repository this script publishes to;
#     1. gh reports the `workflow` scope the push needs (a token that reports
#        no scopes is refused unless --assume-workflow-scope is given), and
#        --remote is github.com/yanyaoer/cua;
#     2. the run is a successful run of linux-sbx-weekly.yml in --repo;
#     3. download its `linux-sbx-release` artifact;
#     4. metadata is well formed and names this run, repository and --branch;
#     5. the bundle's sha256 matches the metadata; SHA256SUMS verifies;
#        every archive and SHA256SUMS has its .sigstore.json/.sig/.pem;
#     6. `gh attestation verify` for the archives, SHA256SUMS, bundle and
#        metadata, signed by this repository's linux-sbx-weekly.yml running
#        from refs/heads/<branch>;
#     7. fetch the upstream tag, `git bundle verify`, the bundle's branch head
#        is the metadata's rebased_sha (local refs only);
#     8. the remote branch is either already at rebased_sha or still at the
#        previous_sha the run started from (else someone pushed meanwhile);
#   write (skipped, only printed, with --dry-run)
#     9. git push --force-with-lease=refs/heads/<branch>:<remote sha now>
#    10. create the prerelease (publish_prerelease.sh) unless a linux-sbx tag
#        already points at rebased_sha and has a release; a tag that is there
#        without a release gets its release on that tag (the tag is neither
#        created nor moved); then read it back (verify_release_assets.sh).
#
# Re-running after a complete publish is a no-op: the pushed branch and the
# existing release are detected and skipped. A linux-sbx tag already at
# rebased_sha without a release (pushed by hand, or its release deleted) gets
# its release on the next run. A publish interrupted inside `gh release
# create` (it uploads assets to a draft, then publishes) is not repaired
# automatically: the read-back fails, and the draft or the incomplete release
# has to be finished or deleted by hand. Nothing here prints a token.
set -euo pipefail

# The one repository this publishes to. The weekly run, its artifact, the
# cosign signatures, the provenance attestations and the prerelease all belong
# to this fork; pushing its rebased branch or creating its release anywhere
# else is a mistake, whatever --repo or the checkout's remote says.
FORK_REPO="yanyaoer/cua"

RUN_ID=""
DRY_RUN=0
ASSUME_WORKFLOW_SCOPE=0
REPO=""
REMOTE="origin"
EXPECT_BRANCH="linux-sbx-patch"
UPSTREAM_URL="https://github.com/trycua/cua"
UPSTREAM_REPO="trycua/cua"
WORKDIR=""
WORKFLOW_FILE="linux-sbx-weekly.yml"
WORKFLOW_NAME="Weekly: Linux sandbox driver"
ARTIFACT="linux-sbx-release"

usage() { sed -n '2,44p' "$0"; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run) DRY_RUN=1; shift ;;
        --repo) REPO="$2"; shift 2 ;;
        --remote) REMOTE="$2"; shift 2 ;;
        --branch) EXPECT_BRANCH="$2"; shift 2 ;;
        --upstream-url) UPSTREAM_URL="$2"; shift 2 ;;
        --upstream-repo) UPSTREAM_REPO="$2"; shift 2 ;;
        --workdir) WORKDIR="$2"; shift 2 ;;
        --assume-workflow-scope) ASSUME_WORKFLOW_SCOPE=1; shift ;;
        -h|--help) usage; exit 0 ;;
        -*) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
        *) [[ -z $RUN_ID ]] || { echo "error: one run id only" >&2; exit 2; }; RUN_ID="$1"; shift ;;
    esac
done
[[ $RUN_ID =~ ^[0-9]+$ ]] || { echo "error: give the numeric Actions run id (got '${RUN_ID}')" >&2; usage >&2; exit 2; }

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
step() { printf '\n== %s\n' "$*"; }
die() { echo "error: $*" >&2; exit 1; }
# Writes go through here: printed always, executed only without --dry-run.
write() {
    printf '+ %s\n' "$*"
    if [[ $DRY_RUN -eq 1 ]]; then
        echo "  (dry run: not executed)"
        return 0
    fi
    "$@"
}

git rev-parse --git-dir >/dev/null 2>&1 || die "run this from a checkout of the fork"
if [[ -z $REPO ]]; then
    REPO="$(gh repo view --json nameWithOwner --jq .nameWithOwner)" || die "cannot tell which repository this is; pass --repo"
fi
[[ $REPO == "$FORK_REPO" ]] \
    || die "this script publishes only $FORK_REPO, not '$REPO' (its run, signatures, attestations and release belong to $FORK_REPO)"
if [[ -z $WORKDIR ]]; then
    WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/linux-sbx-publish.XXXXXX")"
fi
mkdir -p "$WORKDIR"
[[ $DRY_RUN -eq 1 ]] && echo "DRY RUN: verification runs in full; pushes and release writes are only printed."

step "1. gh login and remote"
# --active: only the account gh will actually use, not every stored one.
AUTH="$(gh auth status --active --hostname github.com 2>&1)" || die "gh is not logged in to github.com (gh auth login)"
SCOPES="$(grep -i 'token scopes' <<<"$AUTH" || true)"
if [[ -z $SCOPES ]]; then
    # Fine-grained tokens and some GH_TOKEN setups report no scopes. Not
    # confirmed is treated as missing: the push would fail half-way otherwise.
    if [[ $ASSUME_WORKFLOW_SCOPE -eq 1 ]]; then
        echo "note: gh did not report token scopes; continuing on --assume-workflow-scope"
    else
        die "gh did not report token scopes, so the 'workflow' scope the push needs (the branch deletes workflow files) cannot be confirmed: log in with a classic token that has it (gh auth refresh -s workflow), or pass --assume-workflow-scope if this token can update workflow files"
    fi
elif [[ $SCOPES != *workflow* ]]; then
    die "the gh token lacks the 'workflow' scope the push needs (the branch deletes workflow files): gh auth refresh -s workflow"
else
    echo "ok: token has the workflow scope"
fi
# The configured URL, before any insteadOf rewriting: the branch must go to the
# same repository the run, the attestations and the release belong to.
REMOTE_URL="$(git config --get "remote.${REMOTE}.url")" || die "no remote '$REMOTE' in this checkout"
if [[ ! $REMOTE_URL =~ github\.com[:/]${FORK_REPO//./\.}(\.git)?/?$ ]]; then
    die "remote '$REMOTE' is $REMOTE_URL, not github.com/$FORK_REPO; pass --remote naming it"
fi
echo "ok: $REMOTE is github.com/$FORK_REPO"

step "2. run $RUN_ID in $REPO"
RUN_INFO="$(gh run view "$RUN_ID" --repo "$REPO" --json status,conclusion,workflowName \
    --jq '[.status, .conclusion, .workflowName] | join("|")')" || die "cannot read run $RUN_ID in $REPO"
IFS='|' read -r R_STATUS R_CONCLUSION R_NAME <<<"$RUN_INFO"
[[ $R_NAME == "$WORKFLOW_NAME" ]] || die "run $RUN_ID is '$R_NAME', not '$WORKFLOW_NAME'"
[[ $R_STATUS == completed && $R_CONCLUSION == success ]] \
    || die "run $RUN_ID is $R_STATUS/$R_CONCLUSION; only a successful run is published"
echo "ok: $R_NAME, $R_STATUS/$R_CONCLUSION"

step "3. download $ARTIFACT"
rm -rf "${WORKDIR:?}/release"
gh run download "$RUN_ID" --repo "$REPO" --name "$ARTIFACT" --dir "$WORKDIR/release" \
    || die "cannot download $ARTIFACT from run $RUN_ID"
DIST="$WORKDIR/release/dist"
SRC="$WORKDIR/release/source"
META="$SRC/linux-sbx-metadata.txt"
echo "ok: $(find "$WORKDIR/release" -type f | wc -l | tr -d ' ') file(s) in $WORKDIR/release"

step "4. metadata"
bash "$HERE/metadata.sh" "$META" --check
m() { bash "$HERE/metadata.sh" "$META" "$1"; }
BRANCH="$(m branch)"; REBASED="$(m rebased_sha)"; PREVIOUS="$(m previous_sha)"
TAG="$(m upstream_tag)"; TAG_SHA="$(m upstream_sha)"; BUNDLE="$SRC/$(m bundle)"
PRE_TAG="$(m prerelease_tag)"; PRE_VERSION="$(m prerelease_version)"
[[ "$(m run_id)" == "$RUN_ID" ]] || die "metadata names run $(m run_id), not $RUN_ID"
[[ "$(m repository)" == "$REPO" ]] || die "metadata names repository $(m repository), not $REPO"
[[ $BRANCH == "$EXPECT_BRANCH" ]] || die "metadata names branch $BRANCH, not $EXPECT_BRANCH"
echo "ok: $BRANCH $PREVIOUS -> $REBASED on $TAG; prerelease $PRE_TAG"

step "5. checksums and signature files"
if command -v sha256sum >/dev/null 2>&1; then SHA_CMD=(sha256sum); else SHA_CMD=(shasum -a 256); fi
GOT="$("${SHA_CMD[@]}" "$BUNDLE" | awk '{print $1}')"
[[ $GOT == "$(m bundle_sha256)" ]] || die "bundle sha256 $GOT does not match metadata $(m bundle_sha256)"
(cd "$DIST" && "${SHA_CMD[@]}" -c SHA256SUMS) || die "SHA256SUMS does not verify"
STAGE="cua-driver-rs-${PRE_VERSION}-linux-x86_64"
SUBJECTS=("$DIST/$STAGE.tar.gz" "$DIST/$STAGE-binary.tar.gz" "$DIST/SHA256SUMS" "$BUNDLE" "$META")
for f in "$STAGE.tar.gz" "$STAGE-binary.tar.gz" SHA256SUMS; do
    for ext in "" .sigstore.json .sig .pem; do
        [[ -f "$DIST/$f$ext" ]] || die "artifact lacks dist/$f$ext"
    done
done
echo "ok: bundle sha256, SHA256SUMS, cosign signature files"

step "6. provenance attestations"
for f in "${SUBJECTS[@]}"; do
    gh attestation verify "$f" --repo "$REPO" \
        --signer-workflow "$REPO/.github/workflows/$WORKFLOW_FILE" \
        --source-ref "refs/heads/$EXPECT_BRANCH" >/dev/null \
        || die "attestation check failed for $(basename "$f")"
    echo "ok: $(basename "$f")"
done

step "7. bundle against the upstream tag"
git fetch --quiet --no-tags "$UPSTREAM_URL" "+refs/tags/${TAG}:refs/tags/${TAG}" || die "cannot fetch $TAG from $UPSTREAM_URL"
[[ "$(git rev-parse "${TAG}^{commit}")" == "$TAG_SHA" ]] || die "upstream $TAG is no longer $TAG_SHA"
git bundle verify "$BUNDLE" >/dev/null 2>&1 || die "git bundle verify failed"
HEAD_IN_BUNDLE="$(git bundle list-heads "$BUNDLE" "refs/heads/$BRANCH" | awk '{print $1}')"
[[ $HEAD_IN_BUNDLE == "$REBASED" ]] || die "bundle's $BRANCH is '$HEAD_IN_BUNDLE', metadata says $REBASED"
git fetch --quiet "$BUNDLE" "+refs/heads/${BRANCH}:refs/linux-sbx/incoming"
git merge-base --is-ancestor "$TAG_SHA" "$REBASED" || die "$REBASED is not based on $TAG"
echo "ok: bundle verified; refs/linux-sbx/incoming = $REBASED"

step "8. remote $REMOTE"
REMOTE_SHA="$(git ls-remote "$REMOTE" "refs/heads/$BRANCH" | awk 'NR==1 {print $1}')"
[[ -n $REMOTE_SHA ]] || die "$REMOTE has no refs/heads/$BRANCH"
PUSH=1
if [[ $REMOTE_SHA == "$REBASED" ]]; then
    PUSH=0
    echo "ok: $REMOTE/$BRANCH is already $REBASED; nothing to push"
elif [[ $REMOTE_SHA != "$PREVIOUS" ]]; then
    die "$REMOTE/$BRANCH is $REMOTE_SHA, but run $RUN_ID rebased from $PREVIOUS: someone pushed since; re-run the workflow instead of overwriting it"
else
    echo "ok: $REMOTE/$BRANCH is still $PREVIOUS, as the run expected"
fi
# Every linux-sbx prerelease tag on the remote, with the commit it points at
# (annotated tags: the peeled ^{} line, listed after the tag, wins).
TAG_TARGETS="$(git ls-remote "$REMOTE" 'refs/tags/cua-driver-rs-v*-linux-sbx.*' \
    | awk '{ name = $2; sub("^refs/tags/", "", name); sub("\\^\\{\\}$", "", name); t[name] = $1 }
           END { for (n in t) print n, t[n] }' | sort)"
CREATE=1
EXISTING_TAG=()
RELEASED_AS="$(awk -v sha="$REBASED" -v want="$PRE_TAG" '$2 == sha { if ($1 == want) { print $1; f = 1; exit } any = $1 } END { if (!f && any) print any }' <<<"$TAG_TARGETS")"
if [[ -n $RELEASED_AS ]]; then
    # This commit already has a linux-sbx tag (an unchanged week, or a re-run).
    CANDIDATE="$PRE_TAG"
    PRE_TAG="$RELEASED_AS"
    if VIEW_ERR="$(gh release view "$PRE_TAG" --repo "$REPO" --json tagName 2>&1 >/dev/null)"; then
        CREATE=0
        [[ $PRE_TAG == "$CANDIDATE" ]] || echo "note: $REBASED was already released as $PRE_TAG; not creating $CANDIDATE"
        echo "ok: $PRE_TAG is already released at $REBASED"
    elif [[ $VIEW_ERR == *"release not found"* ]]; then
        # The tag is there (pushed by hand, or its release deleted) but has no
        # release: create the release on that tag, without creating or moving it.
        EXISTING_TAG=(--existing-tag)
        [[ $PRE_TAG == "$CANDIDATE" ]] || echo "note: $REBASED is already tagged $PRE_TAG; not creating $CANDIDATE"
        echo "ok: $PRE_TAG is at $REBASED without a release; the release will be created on it"
    else
        die "cannot tell whether $PRE_TAG has a release: $VIEW_ERR"
    fi
elif awk -v want="$PRE_TAG" '$1 == want { f = 1 } END { exit !f }' <<<"$TAG_TARGETS"; then
    die "$PRE_TAG already exists on $REMOTE for another commit: re-run the workflow for a fresh number"
fi

step "9. push $BRANCH"
if [[ $PUSH -eq 1 ]]; then
    write git push --force-with-lease="refs/heads/${BRANCH}:${REMOTE_SHA}" "$REMOTE" "${REBASED}:refs/heads/${BRANCH}"
fi

step "10. prerelease $PRE_TAG"
if [[ $CREATE -eq 1 ]]; then
    GH_REPO="$REPO" write bash "$HERE/publish_prerelease.sh" \
        --tag "$PRE_TAG" --source-sha "$REBASED" --upstream-tag "$TAG" \
        --upstream-repo "$UPSTREAM_REPO" --repository "$REPO" --branch "$BRANCH" \
        --dist "$DIST" --rebase-result "$(m rebase_result)" --patch-commits "$(m patch_commits)" \
        --summary "" ${EXISTING_TAG[@]+"${EXISTING_TAG[@]}"}
fi
if [[ $DRY_RUN -eq 0 ]]; then
    GH_REPO="$REPO" bash "$HERE/verify_release_assets.sh" --tag "$PRE_TAG" --arch x86_64 --summary ""
fi

step "record"
cat <<EOF
run:        https://github.com/$REPO/actions/runs/$RUN_ID
upstream:   $TAG ($TAG_SHA)
branch:     $BRANCH $PREVIOUS -> $REBASED ($([[ $PUSH -eq 1 ]] && echo pushed || echo already there)$([[ $DRY_RUN -eq 1 ]] && echo ', dry run'))
prerelease: $PRE_TAG ($([[ $CREATE -eq 1 ]] && echo created || echo already released)$([[ $DRY_RUN -eq 1 ]] && echo ', dry run'))
EOF

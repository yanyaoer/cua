#!/usr/bin/env bash
# Put the build output and the source bundle into one release directory and
# check that they belong together (attest job, before signing).
#
#   assemble_release.sh --build-dir B --source-dir S --out OUT
#
# B: the build job's artifact, B/dist/{archives,SHA256SUMS} and
#    B/source/linux-sbx-metadata.txt (the copy the build checked out against).
# S: linux-sbx-source.bundle and linux-sbx-metadata.txt from the rebase job.
# OUT/dist   archives + SHA256SUMS   (what the prerelease ships)
# OUT/source bundle + metadata       (what publish_local.sh pushes)
#
# Refuses (exit 1) when the two metadata files differ, the bundle's sha256 is
# not the recorded one, SHA256SUMS does not verify, or the archives are not
# named for the metadata's prerelease version.
set -euo pipefail

B=""; S=""; OUT=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --build-dir) B="$2"; shift 2 ;;
        --source-dir) S="$2"; shift 2 ;;
        --out) OUT="$2"; shift 2 ;;
        -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
[[ -n $B && -n $S && -n $OUT ]] || { echo "error: --build-dir, --source-dir and --out are required" >&2; exit 2; }

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
META="$S/linux-sbx-metadata.txt"
bash "$HERE/metadata.sh" "$META" --check
cmp -s "$META" "$B/source/linux-sbx-metadata.txt" \
    || { echo "::error::build and rebase metadata differ"; exit 1; }

if command -v sha256sum >/dev/null 2>&1; then SHA_CMD=(sha256sum); else SHA_CMD=(shasum -a 256); fi
BUNDLE="$S/$(bash "$HERE/metadata.sh" "$META" bundle)"
[[ "$("${SHA_CMD[@]}" "$BUNDLE" | awk '{print $1}')" == "$(bash "$HERE/metadata.sh" "$META" bundle_sha256)" ]] \
    || { echo "::error::bundle sha256 does not match the metadata"; exit 1; }

VERSION="$(bash "$HERE/metadata.sh" "$META" prerelease_version)"
STAGE="cua-driver-rs-${VERSION}-linux-x86_64"
for f in "$STAGE.tar.gz" "$STAGE-binary.tar.gz" SHA256SUMS; do
    [[ -f "$B/dist/$f" ]] || { echo "::error::build output lacks $f"; exit 1; }
done
(cd "$B/dist" && "${SHA_CMD[@]}" -c SHA256SUMS >/dev/null) || { echo "::error::SHA256SUMS does not verify"; exit 1; }

rm -rf "$OUT"
mkdir -p "$OUT/dist" "$OUT/source"
cp "$B/dist/$STAGE.tar.gz" "$B/dist/$STAGE-binary.tar.gz" "$B/dist/SHA256SUMS" "$OUT/dist/"
cp "$BUNDLE" "$META" "$OUT/source/"
find "$OUT" -type f | sort

# The run page tells whoever publishes what to do next.
SUMMARY="${GITHUB_STEP_SUMMARY:-}"
if [[ -n $SUMMARY ]]; then
    {
        echo "### Ready to publish \`$(bash "$HERE/metadata.sh" "$META" prerelease_tag)\`"
        echo ""
        echo "Nothing was pushed or released. From a checkout of this fork:"
        echo ""
        echo '```'
        echo ".github/scripts/linux-sbx/publish_local.sh ${GITHUB_RUN_ID:-<run-id>} --dry-run"
        echo ".github/scripts/linux-sbx/publish_local.sh ${GITHUB_RUN_ID:-<run-id>}"
        echo '```'
    } >>"$SUMMARY"
fi

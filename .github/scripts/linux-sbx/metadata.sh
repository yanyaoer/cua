#!/usr/bin/env bash
# Strict reader for linux-sbx-metadata.txt (written by make_source_bundle.sh).
#
#   metadata.sh FILE KEY          print one value
#   metadata.sh FILE --check      validate every field, print nothing
#
# The file comes out of a workflow artifact, so it is parsed, never sourced:
# every line must be key=value with a known key, each key appears once, and
# each value must match that key's format. Exit 1 on any violation.
set -euo pipefail

FILE="${1:?usage: metadata.sh FILE KEY|--check}"
WANT="${2:?usage: metadata.sh FILE KEY|--check}"
[[ -f $FILE ]] || { echo "error: no metadata file at $FILE" >&2; exit 1; }

pattern() {
    case "$1" in
        upstream_tag) echo '^cua-driver-rs-v[0-9]+\.[0-9]+\.[0-9]+$' ;;
        upstream_sha|rebased_sha|previous_sha) echo '^[0-9a-f]{40}$' ;;
        branch) echo '^[A-Za-z0-9._/-]+$' ;;
        bundle) echo '^[A-Za-z0-9._-]+\.bundle$' ;;
        bundle_sha256) echo '^[0-9a-f]{64}$' ;;
        prerelease_tag) echo '^cua-driver-rs-v[0-9]+\.[0-9]+\.[0-9]+-linux-sbx\.[0-9]+$' ;;
        prerelease_version) echo '^[0-9]+\.[0-9]+\.[0-9]+-linux-sbx\.[0-9]+$' ;;
        rebase_result) echo '^(up-to-date|fast-forward|rebased|unknown)$' ;;
        patch_commits) echo '^[0-9]+$' ;;
        run_id) echo '^([0-9]+|local)$' ;;
        repository) echo '^([A-Za-z0-9._-]+/[A-Za-z0-9._-]+|unknown)$' ;;
        *) return 1 ;;
    esac
}

SEEN=" "  # bash 3.2 (macOS) has no associative arrays
VALUE=""
while IFS= read -r line || [[ -n $line ]]; do
    [[ $line =~ ^([a-z_0-9]+)=(.*)$ ]] || { echo "error: malformed metadata line: $line" >&2; exit 1; }
    key="${BASH_REMATCH[1]}"; val="${BASH_REMATCH[2]}"
    re="$(pattern "$key")" || { echo "error: unknown metadata key: $key" >&2; exit 1; }
    [[ $SEEN != *" $key "* ]] || { echo "error: duplicate metadata key: $key" >&2; exit 1; }
    [[ $val =~ $re ]] || { echo "error: bad value for $key: $val" >&2; exit 1; }
    SEEN+="$key "
    [[ $key == "$WANT" ]] && VALUE="$val"
done <"$FILE"

for key in upstream_tag upstream_sha branch rebased_sha previous_sha bundle bundle_sha256 \
           prerelease_tag prerelease_version; do
    [[ $SEEN == *" $key "* ]] || { echo "error: metadata lacks $key" >&2; exit 1; }
done

if [[ $WANT == --check ]]; then
    exit 0
fi
pattern "$WANT" >/dev/null || { echo "error: unknown metadata key: $WANT" >&2; exit 2; }
[[ $SEEN == *" $WANT "* ]] || { echo "error: metadata lacks $WANT" >&2; exit 1; }
printf '%s\n' "$VALUE"

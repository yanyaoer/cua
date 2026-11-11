#!/usr/bin/env bash
# Sign release assets with cosign keyless (GitHub OIDC), no private key.
#
#   sign_release_assets.sh --dir dist
#
# For every asset in --dir (the archives and SHA256SUMS) this writes:
#
#   <file>.sigstore.json  keyless bundle (what install.sh verifies)
#   <file>.sig            detached signature
#   <file>.pem            signing certificate
#
# Needs cosign on PATH and an OIDC token from the job (`id-token: write`).
# Existing signature files are not inputs, so a re-run only re-signs assets.
set -euo pipefail

DIR=""

usage() {
    cat <<'EOF'
usage: sign_release_assets.sh --dir DIR

  --dir DIR   Directory holding the assets to sign (required).
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dir) DIR="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

[[ -n $DIR ]] || { echo "error: --dir is required" >&2; exit 2; }
[[ -d $DIR ]] || { echo "error: $DIR is not a directory" >&2; exit 1; }
command -v cosign >/dev/null || { echo "error: cosign is not on PATH" >&2; exit 1; }

cd "$DIR"
shopt -s nullglob

ASSETS=()
for f in *; do
    case "$f" in
        *.sigstore.json|*.sig|*.pem) ;;
        *) ASSETS+=("$f") ;;
    esac
done

[[ ${#ASSETS[@]} -gt 0 ]] || { echo "error: no assets to sign in $DIR" >&2; exit 1; }

for f in "${ASSETS[@]}"; do
    cosign sign-blob --yes \
        --bundle "$f.sigstore.json" \
        --output-signature "$f.sig" \
        --output-certificate "$f.pem" \
        "$f" >/dev/null
    echo "signed $f"
done

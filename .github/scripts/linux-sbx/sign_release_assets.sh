#!/usr/bin/env bash
# Sign release assets with cosign: keyless (GitHub OIDC), or with a key pair.
#
#   sign_release_assets.sh --dir dist [--key cosign.key]
#
# --key (cluster_build.sh): sign with that private key (COSIGN_PASSWORD
# from the environment, empty when unset), offline: nothing goes to the public
# Rekor log, so verify with the pinned cosign.pub and --insecure-ignore-tlog=true.
# Writes only <file>.sigstore.json (the bundle carries the signature).
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
KEY=""

usage() {
    cat <<'EOF'
usage: sign_release_assets.sh --dir DIR [--key FILE]

  --dir DIR   Directory holding the assets to sign (required).
  --key FILE  cosign private key; offline signature, bundle only.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dir) DIR="$2"; shift 2 ;;
        --key) KEY="$(cd "$(dirname "$2")" && pwd)/$(basename "$2")"; shift 2 ;;
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
    if [[ -n $KEY ]]; then
        COSIGN_PASSWORD="${COSIGN_PASSWORD:-}" cosign sign-blob --yes --key "$KEY" \
            --tlog-upload=false --use-signing-config=false \
            --bundle "$f.sigstore.json" "$f" >/dev/null 2>&1
        echo "signed $f (key)"
        continue
    fi
    cosign sign-blob --yes \
        --bundle "$f.sigstore.json" \
        --output-signature "$f.sig" \
        --output-certificate "$f.pem" \
        "$f" >/dev/null
    echo "signed $f"
done

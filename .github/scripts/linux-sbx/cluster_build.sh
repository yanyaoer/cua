#!/usr/bin/env bash
# Build one linux-sbx release on a Linux build host, no CI.
#
#   cluster_build.sh --work DIR --key COSIGN_KEY [--repo URL] [--branch B]
#                    (--branch: start from origin/B instead of linux-sbx, for a trial)
#                    [--upstream-tag cua-driver-rs-vX.Y.Z] [--jobs N] [--mirror-env FILE]
#
# The whole release in one place: select the upstream tag, name the
# prerelease, rebase linux-sbx onto the tag, prune, bundle, build in a
# Debian 11 root (glibc 2.31 floor) under proot, pack, assemble, and sign every
# asset with the cosign key pair. It pushes nothing and uploads nothing: where
# the release goes (branch, tag, artifact store) is the build host's business,
# from the release directory printed last.
#
# --mirror-env FILE: KEY=VALUE lines naming download proxies for hosts where
# the upstream servers are slow or unreachable. Only these keys are read (the
# file is parsed, never sourced): LINUX_SBX_DEBIAN_MIRROR, LINUX_SBX_APT_MIRROR,
# LINUX_SBX_NODE_MIRROR, LINUX_SBX_RUSTUP_MIRROR, RUSTUP_DIST_SERVER,
# RUSTUP_UPDATE_ROOT, LINUX_SBX_CARGO_MIRROR (a sparse+ registry index URL),
# npm_config_registry. Without it every download goes direct. sha256 pins, the
# Debian keyring and Cargo.lock checksums still decide what is accepted, so a
# mirror changes where bytes come from, not what is built.
#
# DIR keeps the rootfs cache (DIR/cache/rootfs.tar.gz, reused while its stamp
# matches) and one directory per run (DIR/runs/<run id>/). Needs NOPASSWD sudo
# (debootstrap and the build root are root-owned), git, curl, cosign.
#
# Exit 0 with the release in DIR/runs/<id>/release, 10 on a rebase conflict
# (report in DIR/runs/<id>/out/linux-sbx-rebase-conflict.md), 11 when the
# rebased tree builds but the extension bridge tests fail (bridge_tests.sh:
# nothing is signed), 1 otherwise.
set -euo pipefail

WORK=""; KEY=""; REPO="https://github.com/yanyaoer/cua.git"; BRANCH="linux-sbx"
UPSTREAM_TAG=""; JOBS=32; MIRROR_FILE=""
UPSTREAM_URL=https://github.com/trycua/cua
REPOSITORY=yanyaoer/cua
MIRROR_KEYS="LINUX_SBX_DEBIAN_MIRROR LINUX_SBX_APT_MIRROR LINUX_SBX_NODE_MIRROR LINUX_SBX_RUSTUP_MIRROR RUSTUP_DIST_SERVER RUSTUP_UPDATE_ROOT LINUX_SBX_CARGO_MIRROR npm_config_registry"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --work) WORK="$2"; shift 2 ;;
        --key) KEY="$2"; shift 2 ;;
        --repo) REPO="$2"; shift 2 ;;
        --branch) BRANCH="$2"; shift 2 ;;
        --upstream-tag) UPSTREAM_TAG="$2"; shift 2 ;;
        --jobs) JOBS="$2"; shift 2 ;;
        --mirror-env) MIRROR_FILE="$2"; shift 2 ;;
        -h|--help) sed -n '2,31p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
[[ -n $WORK && -n $KEY ]] || { echo "error: --work and --key are required" >&2; exit 2; }
[[ -r $KEY ]] || { echo "error: cannot read the cosign key $KEY" >&2; exit 1; }
[[ -z $MIRROR_FILE || -r $MIRROR_FILE ]] || { echo "error: cannot read $MIRROR_FILE" >&2; exit 1; }
command -v cosign >/dev/null || { echo "error: cosign is not on PATH" >&2; exit 1; }
sudo -n true 2>/dev/null || { echo "error: needs NOPASSWD sudo" >&2; exit 1; }

WORK="$(mkdir -p "$WORK" && cd "$WORK" && pwd)"
RUN_ID="$(date -u +%Y%m%d%H%M%S)"
W="$WORK/runs/$RUN_ID"
mkdir -p "$W/out" "$WORK/cache"
exec > >(tee -a "$W/build.log") 2>&1
echo "run $RUN_ID in $W"
step() { echo "== $(date -u +%H:%M:%S) $*"; }

retry() { # n cmd... (github.com drops some connections from this cluster)
    local n="$1" i; shift
    for ((i = 1; i <= n; i++)); do
        "$@" && return 0
        [[ $i -lt $n ]] && { echo "attempt $i of $n failed; retrying in $((i * 15))s" >&2; sleep $((i * 15)); }
    done
    return 1
}

step "checkout $REPO $BRANCH"
retry 3 git clone --quiet --no-checkout "$REPO" "$W/repo"
cd "$W/repo"
git config http.version HTTP/1.1
PATCH=linux-sbx  # the name the metadata, the bundle and the publish use
git checkout --quiet -B "$PATCH" "origin/$BRANCH"
S=.github/scripts/linux-sbx
get() { grep -m1 "^$2=" "$W/out/$1.env" | cut -d= -f2-; }

step "select upstream tag"
ARGS=(--remote-url "$UPSTREAM_URL" --output "$W/out/select.env")
[[ -n $UPSTREAM_TAG ]] && ARGS+=(--exact "$UPSTREAM_TAG")
retry 3 "$S/select_upstream_tag.sh" "${ARGS[@]}"
# Published prerelease tags are this repository's own: the clone has them all.
"$S/next_prerelease_tag.sh" --version "$(get select version)" --remote-url "$W/repo" --output "$W/out/prerelease.env"

step "fetch upstream cua-driver-rs-v* tags"
retry 4 git fetch --quiet --force --no-tags "$UPSTREAM_URL" "+refs/tags/cua-driver-rs-v*:refs/tags/cua-driver-rs-v*"

step "rebase onto $(get select tag)"
set +e
"$S/rebase_onto_tag.sh" --branch "$PATCH" --upstream-ref "$(get select tag)" \
    --conflict-report "$W/out/linux-sbx-rebase-conflict.md" \
    --output "$W/out/rebase.env" --summary "$W/out/summary.md"
rc=$?
set -e
if [[ $rc -eq 10 ]]; then
    echo "rebase conflict; report: $W/out/linux-sbx-rebase-conflict.md" >&2
    cat "$W/out/linux-sbx-rebase-conflict.md" >&2 || true
    exit 10
fi
[[ $rc -eq 0 ]] || exit 1
"$S/prune_upstream_workflows.sh" --output "$W/out/prune.env"
"$S/make_source_bundle.sh" --branch "$PATCH" \
    --upstream-tag "$(get select tag)" \
    --previous-sha "$(get rebase previous_sha)" \
    --prerelease-tag "$(get prerelease tag)" \
    --rebase-result "$(get rebase result)" \
    --patch-commits "$(get rebase patch_commits)" \
    --run-id "$RUN_ID" --repository "$REPOSITORY" \
    --out "$W/source"
# The scripts of the rebased tree build it (as the pipelines did); a name
# upstream does not use (upstream has scripts/ at the top).
cp -R "$S" "$W/sbx-scripts"
SHA="$(get prune sha)"
VERSION="$(get prerelease prerelease)"
cd "$W"

step "Debian 11 build root"
MIRROR_ENV=()
if [[ -n $MIRROR_FILE ]]; then
    while IFS= read -r line || [[ -n $line ]]; do
        line="${line%$'\r'}"
        [[ -z $line || $line == \#* ]] && continue
        k="${line%%=*}"; v="${line#*=}"
        [[ $line == *=* && " $MIRROR_KEYS " == *" $k "* ]] \
            || { echo "error: $MIRROR_FILE: unexpected line for key '$k'" >&2; exit 1; }
        [[ -n $v ]] && MIRROR_ENV+=("$k=$v")
    done <"$MIRROR_FILE"
    echo "mirrors: ${#MIRROR_ENV[@]} set from $MIRROR_FILE"
fi
# The crate cache outlives the run: later weeks download only what changed.
mkdir -p "$WORK/cargo-registry"
sudo env "${MIRROR_ENV[@]}" bash "$W/sbx-scripts/debian11_rootfs.sh" --dir "$WORK/rootfs" --cache-tar "$WORK/cache/rootfs.tar.gz"

step "source from the bundle"
retry 4 bash "$W/sbx-scripts/checkout_from_bundle.sh" --source-dir "$W/source" --upstream-url "$UPSTREAM_URL" \
    --dest "$W/src" --copy-metadata-to "$W/build-out/source"

step "build $VERSION ($SHA), $JOBS jobs"
sudo env "${MIRROR_ENV[@]}" CARGO_BUILD_JOBS="$JOBS" CUA_DRIVER_GIT_SHA="$SHA" \
    bash "$W/sbx-scripts/debian11_proot.sh" --root "$WORK/rootfs" --bind "$W:/work" \
    --bind "$WORK/cargo-registry:/root/.cargo/registry" --cwd /work/src -- /bin/bash -euo pipefail -c '
    cd libs/cua-driver/rust
    MIRROR_CFG=()
    if [[ -n ${LINUX_SBX_CARGO_MIRROR:-} ]]; then
        MIRROR_CFG=(--config "source.crates-io.replace-with=\"intranet\"" --config "source.intranet.registry=\"$LINUX_SBX_CARGO_MIRROR\"")
    fi
    cargo "${MIRROR_CFG[@]}" build --locked -p cua-driver -p cursor-theme-cli -p cua-driver-sdk --release \
        --features cua-driver/portal-input --target x86_64-unknown-linux-gnu
    cd ..
    npm --prefix typescript ci --ignore-scripts
    node scripts/build-node-runtime.mjs --target x86_64-unknown-linux-gnu \
        --output rust/target/x86_64-unknown-linux-gnu/release/cua_driver_node_runtime.node
    cd /work/src
    bash /work/sbx-scripts/pack_linux_artifact.sh --version "'"$VERSION"'" --arch x86_64 \
        --target x86_64-unknown-linux-gnu --rust-dir /work/src/libs/cua-driver/rust \
        --out-dir /work/build-out/dist --smoke
    ldd --version 2>&1 | sed -n 1p  # not `head -1`: under pipefail ldd gets SIGPIPE (rc 141)
'

# A clean rebase only means the patches still apply as text; the bridge tests
# say whether the bridge still works on what upstream wrote. No signature
# without them.
step "bridge gate"
set +e
sudo env "${MIRROR_ENV[@]}" CARGO_BUILD_JOBS="$JOBS" CUA_DRIVER_GIT_SHA="$SHA" \
    bash "$W/sbx-scripts/debian11_proot.sh" --root "$WORK/rootfs" --bind "$W:/work" \
    --bind "$WORK/cargo-registry:/root/.cargo/registry" --cwd /work/src -- \
    bash /work/sbx-scripts/bridge_tests.sh --rust-dir /work/src/libs/cua-driver/rust \
    --target x86_64-unknown-linux-gnu --features cua-driver/portal-input
rc=$?
set -e
sudo chown -R "$(id -u):$(id -g)" "$W"
if [[ $rc -eq 11 ]]; then
    echo "bridge gate failed: the release is not signed" >&2
    exit 11
fi
[[ $rc -eq 0 ]] || { echo "error: bridge gate exited $rc" >&2; exit 1; }

step "assemble and sign"
bash "$W/sbx-scripts/assemble_release.sh" --build-dir "$W/build-out" --source-dir "$W/source" --out "$W/release"
bash "$W/sbx-scripts/sign_release_assets.sh" --dir "$W/release/dist" --key "$KEY"
cp "$W/out/summary.md" "$W/release/" 2>/dev/null || true
step "done"
echo "release=$W/release"
echo "version=$VERSION"
echo "sha=$SHA"

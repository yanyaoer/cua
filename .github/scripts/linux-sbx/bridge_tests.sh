#!/usr/bin/env bash
# Run the extension bridge's tests on the rebased tree: the release gate.
#
#   bridge_tests.sh --rust-dir DIR --target TRIPLE [--features F]
#
# A clean `git rebase` only says the patch still applies as text. Upstream can
# still break the bridge without touching a patched line: a new DevTools port
# path in the Linux adapter, a changed CDP demultiplexer, a moved hook. These
# tests are the check that it still works; a release is published only when
# they pass. Runs inside the Debian 11 build root after the release build, with
# the same target triple and features, so the dependencies are reused.
#
# Covered: browser::ext_bridge (frames, config, host, listener, peer, endpoint,
# engine tests against a fake extension), browser::wire, browser::cdp_ws (the
# demultiplexer the bridge runs over), the Linux adapter's hook guard and the
# native host CLI test.
#
# Exit 0 all passed, 11 a bridge test failed, 2 bad usage.
set -euo pipefail

RUST_DIR=""; TARGET=""; FEATURES=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --rust-dir) RUST_DIR="$2"; shift 2 ;;
        --target) TARGET="$2"; shift 2 ;;
        --features) FEATURES="$2"; shift 2 ;;
        -h|--help) sed -n '2,18p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
[[ -n $RUST_DIR && -n $TARGET ]] || { echo "error: --rust-dir and --target are required" >&2; exit 2; }
cd "$RUST_DIR"

# Same profile and target as the release build, so most dependencies are
# reused. --features names the release build's features (cua-driver/portal-input);
# cargo accepts them only where that package is selected, so they go to the
# cua-driver run. The cargo mirror (when set) is passed the same way
# cluster_build.sh does.
ARGS=(--locked --release --target "$TARGET")
DRIVER_ARGS=()
[[ -n $FEATURES ]] && DRIVER_ARGS+=(--features "$FEATURES")
MIRROR_CFG=()
if [[ -n ${LINUX_SBX_CARGO_MIRROR:-} ]]; then
    MIRROR_CFG=(--config "source.crates-io.replace-with=\"intranet\"" --config "source.intranet.registry=\"$LINUX_SBX_CARGO_MIRROR\"")
fi

fail=0
run() { # label cargo-test-args...
    local label="$1"; shift
    echo "== bridge gate: $label"
    if ! cargo "${MIRROR_CFG[@]}" test "${ARGS[@]}" "$@"; then
        echo "bridge gate FAILED: $label" >&2
        fail=1
    fi
}
run "core: ext_bridge, wire, cdp_ws" -p cua-driver-core --lib -- \
    browser::ext_bridge:: browser::wire:: browser::cdp_ws::
run "platform-linux: bridge hook guard" -p platform-linux --lib -- ext_bridge_hook_tests
run "cua-driver: native host CLI" -p cua-driver "${DRIVER_ARGS[@]}" --test ext_host_cli_test

if [[ $fail -ne 0 ]]; then
    echo "bridge gate: failed" >&2
    exit 11
fi
echo "bridge gate: passed"

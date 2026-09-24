#!/usr/bin/env bash
# Run the in-runtime `DesktopApi` e2e battery (`crates/inka-runtime/src/desktop_e2e.rs`)
# against a real laufey backend.
#
# Usage: scripts/ci/desktop-e2e.sh
#
# env:
#   INKA_LAUFEY_BACKEND   use an existing laufey backend binary (skips lookup)
#   DESKTOP_E2E_PROFILE   cargo profile for the runtime build (default: debug)
#   DESKTOP_E2E_FETCH     fetch the pinned CEF backend when missing (default: 0)
#   DESKTOP_E2E_STRICT    fail instead of skipping when prerequisites are absent
#   LIBCLANG_PATH         bindgen needs libclang (default set below if unset)
#
# On Linux the battery must run under CEF: the WebKitGTK webview backend is not
# thread-safe under the worker-thread runtime and is excluded headless. The run
# is wrapped in `xvfb-run` + `dbus-run-session` (tray/notification code paths
# need a session bus).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
PROFILE="${DESKTOP_E2E_PROFILE:-dev}"
TARGET="${DESKTOP_E2E_TARGET:-x86_64-unknown-linux-gnu}"
STRICT="${DESKTOP_E2E_STRICT:-0}"

# Map the cargo profile name to its flag and target subdirectory.
case "$PROFILE" in
    dev | debug)
        profile_flag=""
        profile_dir="debug"
        ;;
    release)
        profile_flag="--release"
        profile_dir="release"
        ;;
    *)
        profile_flag="--profile $PROFILE"
        profile_dir="$PROFILE"
        ;;
esac

skip() {
    if [ "$STRICT" = "1" ]; then
        echo "error: $*" >&2
        exit 1
    fi
    echo "skip: $*"
    exit 0
}

if [ "$(uname -s)" != "Linux" ]; then
    skip "desktop-e2e is Linux-only (found $(uname -s))"
fi

if [ "$STRICT" != "1" ] && ! command -v xvfb-run >/dev/null 2>&1; then
    skip "xvfb-run not installed (headless desktop test)"
fi
if [ "$STRICT" != "1" ] && ! command -v dbus-run-session >/dev/null 2>&1; then
    skip "dbus-run-session not installed (tray/notification test)"
fi

# bindgen (laufey) needs libclang; provide the common Debian/Ubuntu location.
if [ -z "${LIBCLANG_PATH:-}" ] && [ -e /usr/lib/x86_64-linux-gnu/libclang.so ]; then
    export LIBCLANG_PATH=/usr/lib/x86_64-linux-gnu
fi

echo "== building the desktop-e2e runtime (profile: $PROFILE) =="
# shellcheck disable=SC2086
cargo build --locked -p inka-runtime --features desktop,desktop-e2e $profile_flag

runtime_so="$TARGET_DIR/$profile_dir/libinka_runtime.so"
[ -f "$runtime_so" ] || { echo "error: runtime not built: $runtime_so" >&2; exit 1; }

# --- resolve a laufey backend ------------------------------------------------
backend="${INKA_LAUFEY_BACKEND:-}"

laufe_version="$(sed -n 's/.*const LAUFEY_VERSION: &str = "\([0-9][0-9.]*\)".*/\1/p' \
    "$ROOT/crates/inka/src/desktop.rs" | head -1)"
[ -n "$laufe_version" ] || { echo "error: could not parse LAUFEY_VERSION" >&2; exit 1; }

cef_dir_for() { # <cache-root>
    echo "$1/$laufe_version/cef/$TARGET"
}

find_cached_backend() {
    local xdg="${XDG_CACHE_HOME:-$HOME/.cache}"
    for root in \
        "$xdg/inka/laufey" \
        "${DENO_DIR:-$HOME/.cache/deno}/laufey" \
        "$HOME/.cache/deno/laufey" \
        "$HOME/.cache/inka/laufey"; do
        local dir
        dir="$(cef_dir_for "$root")"
        if [ -x "$dir/laufey" ]; then
            echo "$dir/laufey"
            return 0
        fi
    done
    return 1
}

if [ -z "$backend" ]; then
    backend="$(find_cached_backend || true)"
fi

if [ -z "$backend" ] || [ ! -x "$backend" ]; then
    if [ "${DESKTOP_E2E_FETCH:-0}" != "1" ]; then
        skip "no CEF laufey backend found (set INKA_LAUFEY_BACKEND or DESKTOP_E2E_FETCH=1)"
    fi
    read -r archive sha < <(bash "$ROOT/scripts/ci/laufey-asset.sh" cef "$TARGET")
    dest_root="${XDG_CACHE_HOME:-$HOME/.cache}/inka/laufey"
    dest="$(cef_dir_for "$dest_root")"
    mkdir -p "$dest"
    tmp="$dest/.$archive.tmp$$"
    echo "== downloading $archive (laufey $laufe_version) =="
    curl -fsSL --proto '=https' --tlsv1.2 \
        "https://github.com/littledivy/laufey/releases/download/v$laufe_version/$archive" \
        -o "$tmp"
    got="$(sha256sum "$tmp" | awk '{print $1}')"
    [ "$got" = "$sha" ] || { rm -f "$tmp"; echo "error: checksum mismatch ($got != $sha)" >&2; exit 1; }
    tar -xzf "$tmp" --no-same-owner --no-same-permissions -C "$dest"
    rm -f "$tmp"
    backend="$dest/laufey"
    [ -x "$backend" ] || { echo "error: $backend not found after extract" >&2; exit 1; }
fi

echo "== desktop-e2e: backend=$backend runtime=$runtime_so =="
set +e
INKA_DESKTOP_E2E=1 LAUFEY_RUNTIME_PATH="$runtime_so" \
    xvfb-run -a dbus-run-session -- "$backend" --runtime "$runtime_so"
status=$?
set -e
exit "$status"

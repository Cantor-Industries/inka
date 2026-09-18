#!/usr/bin/env bash
# Assert the runtime's exported inka_runtime_* symbol set — the frozen C ABI
# the launcher and `inka run` dlopen. Fast; run after building the runtime.
#
# The ABI is additive: a release may add symbols, but removing or renaming a
# required one is a breaking tuple event. This gate makes that explicit.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LIB="${1:-}"

if [ -z "$LIB" ]; then
    TARGET="${CARGO_TARGET_DIR:-$ROOT/target}/release"
    LIB="$TARGET/libinka_runtime.so"
fi

if [ ! -f "$LIB" ]; then
    echo "error: runtime not found at $LIB (build inka-runtime first)" >&2
    exit 1
fi

if ! command -v nm >/dev/null 2>&1; then
    echo "error: nm (binutils) is required to inspect $LIB" >&2
    exit 1
fi

have="$(nm -D --defined-only "$LIB" | awk '{print $NF}' | grep -E '^inka_runtime_' | sort -u || true)"

required=(
    inka_runtime_version
    inka_runtime_create
    inka_runtime_destroy
    inka_runtime_run_module_dir
    inka_runtime_free_string
    inka_runtime_features
)

missing=()
for sym in "${required[@]}"; do
    if ! printf '%s\n' "$have" | grep -qx "$sym"; then
        missing+=("$sym")
    fi
done

if [ "${#missing[@]}" -ne 0 ]; then
    echo "error: runtime is missing required ABI symbol(s): ${missing[*]}" >&2
    echo "exported inka_runtime_* symbols:" >&2
    printf '%s\n' "$have" | sed 's/^/  /' >&2
    exit 1
fi

echo "ABI symbols OK:"
printf '%s\n' "$have" | sed 's/^/  /'

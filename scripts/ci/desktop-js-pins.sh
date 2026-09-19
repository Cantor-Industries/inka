#!/usr/bin/env bash
# Verify the vendored desktop JS is re-synced when the engine or laufey pin
# moves. `crates/inka-runtime/src/desktop_js.rs` is copied from Deno's
# `cli/rt/desktop.rs` (byte-for-byte except for the `serde_json` path); when
# `deno_runtime` or `laufey` is bumped, it must be re-vendored and these
# recorded versions updated. Fast (no build); run on every PR.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
JS="$ROOT/crates/inka-runtime/src/desktop_js.rs"
TOML="$ROOT/crates/inka-runtime/Cargo.toml"

[ -f "$JS" ] || { echo "error: missing $JS" >&2; exit 1; }
[ -f "$TOML" ] || { echo "error: missing $TOML" >&2; exit 1; }

# The engine and laufey the vendored JS was last synced from. Bump these in the
# same commit that re-vendors desktop_js.rs.
VENDORED_DENO_RUNTIME="0.267.0"
VENDORED_LAUFEY="0.7.0"

DENO="$(sed -n 's/.*deno_runtime = { version = "=\([0-9][0-9.]*\)".*/\1/p' "$TOML" | head -1)"
LAUFEY="$(sed -n 's/.*laufey = { version = "=\([0-9][0-9.]*\)".*/\1/p' "$TOML" | head -1)"
[ -n "$DENO" ] || { echo "error: could not parse the deno_runtime pin" >&2; exit 1; }
[ -n "$LAUFEY" ] || { echo "error: could not parse the laufey pin" >&2; exit 1; }

status=0
if [ "$DENO" != "$VENDORED_DENO_RUNTIME" ]; then
    echo "error: desktop_js.rs was vendored from deno_runtime $VENDORED_DENO_RUNTIME but" >&2
    echo "       the crate pins $DENO. Re-sync it from Deno's cli/rt/desktop.rs and" >&2
    echo "       update VENDORED_DENO_RUNTIME in $0." >&2
    status=1
fi
if [ "$LAUFEY" != "$VENDORED_LAUFEY" ]; then
    echo "error: desktop_js.rs targets laufey $VENDORED_LAUFEY but the crate pins $LAUFEY." >&2
    echo "       Re-vendor it and update VENDORED_LAUFEY in $0." >&2
    status=1
fi

# The desktop ops the vendored JS calls are declared in the same script; make
# sure the symbols it depends on are still present (guards a partial re-sync).
for sym in op_desktop_apply_patch op_desktop_verify_ed25519 op_desktop_send_error_report; do
    if ! grep -q "$sym" "$JS"; then
        echo "error: desktop_js.rs is missing expected symbol $sym" >&2
        status=1
    fi
done

[ "$status" -eq 0 ] || exit 1
echo "desktop JS pins OK (deno_runtime $DENO, laufey $LAUFEY)"

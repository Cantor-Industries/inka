#!/usr/bin/env bash
# Verify the Deno pin invariants that keep a runtime tuple bump a contained
# change. Fast (no build); run on every PR.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CARGO_TOML="$ROOT/crates/inka-runtime/Cargo.toml"
RUNTIME_VERSION_FILE="$ROOT/crates/inka-runtime/runtime-version"

[ -f "$CARGO_TOML" ] || { echo "error: missing $CARGO_TOML" >&2; exit 1; }
[ -f "$RUNTIME_VERSION_FILE" ] || { echo "error: missing $RUNTIME_VERSION_FILE" >&2; exit 1; }

# The runtime tuple version (deno_runtime base + inka revision), e.g. 0.266.4.
RUNTIME="$(tr -d '[:space:]' < "$RUNTIME_VERSION_FILE")"
[ -n "$RUNTIME" ] || { echo "error: $RUNTIME_VERSION_FILE is empty" >&2; exit 1; }

# deno_runtime is pinned exactly: deno_runtime = { version = "=0.266.0", ...
DENO="$(sed -n 's/.*deno_runtime = { version = "=\([0-9][0-9.]*\)".*/\1/p' "$CARGO_TOML" | head -1)"
[ -n "$DENO" ] || { echo "error: could not parse the deno_runtime pin from $CARGO_TOML" >&2; exit 1; }

# The tuple's major.minor must match the pinned deno_runtime base. A prerelease
# suffix (`0.267.2-beta.1`) is stripped first.
rt_base="$RUNTIME"
case "$RUNTIME" in
    *-beta.*) rt_base="${RUNTIME%%-beta.*}" ;;
    *-rc.*) rt_base="${RUNTIME%%-rc.*}" ;;
esac
runtime_base="${rt_base%.*}"
deno_base="${DENO%.*}"
if [ "$runtime_base" != "$deno_base" ]; then
    echo "error: runtime-version $RUNTIME does not share the deno_runtime base $DENO" >&2
    exit 1
fi

# Every Deno-project dependency must be an exact (=) pin, in every crate that
# names one (the runtime engine and the bundler). Match generically so a newly
# added crate (e.g. another rolldown_* or deno_*) is enforced too.
PIN_PATTERN='^(rolldown(_[a-z0-9_]+)?|deno_[a-z0-9_]+|node_resolver|import_map|sys_traits)[[:space:]]*='
for toml in "$ROOT/crates/inka-runtime/Cargo.toml" "$ROOT/crates/inka-bundler/Cargo.toml"; do
    [ -f "$toml" ] || continue
    while IFS= read -r line; do
        [ -n "$line" ] || continue
        dep="${line%%=*}"
        dep="$(printf '%s' "$dep" | tr -d '[:space:]')"
        case "$line" in
            *'"='*) ;;
            *)
                echo "error: $dep in $toml is not pinned exactly (expected \"=<version>\")" >&2
                exit 1
                ;;
        esac
    done < <(grep -E "$PIN_PATTERN" "$toml" || true)
done

echo "deno pins OK (runtime $RUNTIME, deno_runtime $DENO)"

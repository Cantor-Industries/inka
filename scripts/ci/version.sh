#!/usr/bin/env bash
# Release version discovery for the tag-triggered CI.
# Prints: release=<tag minus v>  runtime=<runtime tuple version>
#         deno=<deno_runtime base pin>
#
# usage: version.sh <tag>
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TAG="${1:?usage: version.sh <tag>}"

REL="${TAG#v}"
case "$REL" in
    ""|*[!0-9A-Za-z.+~-]*) echo "error: invalid release tag '$TAG'" >&2; exit 1 ;;
esac

# The `inka` crate version must match the release tag (the toolchain version
# tracks it). Fail fast rather than publishing a mismatched toolchain.
CRATE="$(sed -n 's/^version = "\([0-9][0-9.]*\)"$/\1/p' "$ROOT/crates/inka/Cargo.toml" | head -1)"
[ -n "$CRATE" ] || { echo "error: could not parse the inka crate version" >&2; exit 1; }
[ "$CRATE" = "$REL" ] || { echo "error: tag '$TAG' does not match inka crate version '$CRATE'" >&2; exit 1; }

# The runtime tuple version (deno_runtime base + inka revision), e.g. 0.266.4.
RUNTIME="$(tr -d '[:space:]' < "$ROOT/crates/inka-runtime/runtime-version" 2>/dev/null || true)"
[ -n "$RUNTIME" ] || { echo "error: could not read crates/inka-runtime/runtime-version" >&2; exit 1; }
# This value is emitted into the release workflow; keep it to a dotted numeric
# version so it can never carry shell metacharacters (the workflow no longer
# `eval`s it, but the invariant is worth enforcing at the source).
case "$RUNTIME" in
    *[!0-9.]*) echo "error: runtime-version '$RUNTIME' is not a dotted numeric version" >&2; exit 1 ;;
esac

# deno_runtime is pinned exactly: deno_runtime = { version = "=0.266.0", ...
DENO="$(sed -n 's/.*deno_runtime = { version = "=\([0-9][0-9.]*\)".*/\1/p' \
    "$ROOT/crates/inka-runtime/Cargo.toml" | head -1 || true)"
[ -n "$DENO" ] || { echo "error: could not parse deno_runtime version" >&2; exit 1; }

echo "release=$REL"
echo "runtime=$RUNTIME"
echo "deno=$DENO"

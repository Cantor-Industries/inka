#!/usr/bin/env bash
# Release version discovery for the tag-triggered CI.
# Prints: release=<tag minus v>  runtime=<runtime tuple version>
#         deno=<deno_runtime base pin>  resolver=<resolver crate version>
#
# usage: version.sh <tag>
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TAG="${1:?usage: version.sh <tag>}"

REL="${TAG#v}"
case "$REL" in
    ""|*[!0-9A-Za-z.+~-]*) echo "error: invalid release tag '$TAG'" >&2; exit 1 ;;
esac

# The runtime tuple version (deno_runtime base + inka revision), e.g. 0.266.2.
RUNTIME="$(tr -d '[:space:]' < "$ROOT/crates/inka-runtime/runtime-version" 2>/dev/null || true)"
[ -n "$RUNTIME" ] || { echo "error: could not read crates/inka-runtime/runtime-version" >&2; exit 1; }

# deno_runtime is pinned exactly: deno_runtime = { version = "=0.266.0", ...
DENO="$(sed -n 's/.*deno_runtime = { version = "=\([0-9][0-9.]*\)".*/\1/p' \
    "$ROOT/crates/inka-runtime/Cargo.toml" | head -1 || true)"
[ -n "$DENO" ] || { echo "error: could not parse deno_runtime version" >&2; exit 1; }

RES="$(sed -n 's/^version = "\([0-9][0-9.]*\)"$/\1/p' \
    "$ROOT/crates/inka-resolver/Cargo.toml" | head -1 || true)"
[ -n "$RES" ] || RES="1.0.0"

echo "release=$REL"
echo "runtime=$RUNTIME"
echo "deno=$DENO"
echo "resolver=$RES"

#!/usr/bin/env bash
# Release version discovery for the tag-triggered CI.
# Prints:
#   release=<toolchain version, e.g. 0.8.1 or 0.8.1-beta.2>
#   runtime=<runtime tuple version>
#   deno=<deno_runtime base pin>
#   channel=<stable|beta>
#   base=<target release, e.g. 0.8.1>
#   tag=<the git tag>
#
# usage: version.sh <tag>
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TAG="${1:?usage: version.sh <tag>}"

REL="${TAG#v}"
case "$REL" in
    ""|*[!0-9A-Za-z.+~-]*) echo "error: invalid release tag '$TAG'" >&2; exit 1 ;;
esac

# Beta tags are `v<base>-beta.<n>-<short-hash>` (rc is accepted symmetrically).
# `REL` is the full suffix; `REL_V` is the orderable toolchain version and
# `BASE` the target release (what the crate version must equal).
CHANNEL=stable
BASE="$REL"
REL_V="$REL"
case "$REL" in
    *-beta*|*-rc*) CHANNEL=beta ;;
esac
if [ "$CHANNEL" = beta ]; then
        case "$REL" in
            *-beta.*-*|*-rc.*-*) ;;
            *) echo "error: beta tag '$TAG' must look like v<ver>-beta.<n>-<short-hash>" >&2; exit 1 ;;
        esac
        # `strip the counter/hash`: 0.8.1-beta.2-49efc7f -> 0.8.1-beta.2
        REL_V="${REL%-*}"
        # `BASE`: 0.8.1-beta.2 -> 0.8.1
        BASE="${REL_V%%-beta.*}"
        [ "$BASE" != "$REL_V" ] || BASE="${REL_V%%-rc.*}"
        # The trailing piece must be a hex commit hash.
        HASH="${REL##*-}"
        case "$HASH" in
            ""|*[!0-9a-fA-F]*) echo "error: beta tag '$TAG' has a non-hex commit suffix" >&2; exit 1 ;;
        esac
        # `REL_V` must be a dotted numeric version plus `-beta.N`/`-rc.N`.
        _pre="${REL_V#"$BASE"}"
        case "$BASE" in
            ""|*[!0-9.]*) echo "error: beta tag '$TAG' has an invalid base '$BASE'" >&2; exit 1 ;;
        esac
        case "$_pre" in
            -beta.[0-9]*|-rc.[0-9]*) ;;
            *) echo "error: beta tag '$TAG' has an invalid prerelease '$_pre'" >&2; exit 1 ;;
        esac
fi

# The `inka` crate version must match the target release (the toolchain version
# tracks it). A beta shares the target version, so only `BASE` is compared.
CRATE="$(sed -n 's/^version = "\([0-9][0-9.]*\)"$/\1/p' "$ROOT/crates/inka/Cargo.toml" | head -1)"
[ -n "$CRATE" ] || { echo "error: could not parse the inka crate version" >&2; exit 1; }
[ "$CRATE" = "$BASE" ] || { echo "error: tag '$TAG' (base $BASE) does not match inka crate version '$CRATE'" >&2; exit 1; }

# The runtime tuple version (deno_runtime base + inka revision), e.g. 0.266.4 or
# 0.266.4-beta.1.
RUNTIME="$(tr -d '[:space:]' < "$ROOT/crates/inka-runtime/runtime-version" 2>/dev/null || true)"
[ -n "$RUNTIME" ] || { echo "error: could not read crates/inka-runtime/runtime-version" >&2; exit 1; }
# Keep the value to a dotted numeric version with an optional prerelease so it
# can never carry shell metacharacters (the workflow does not `eval` it, but the
# invariant is worth enforcing at the source).
RT_BASE="$RUNTIME"
case "$RUNTIME" in
    *-beta.*|*-rc.*) RT_BASE="${RUNTIME%%-beta.*}"; [ "$RT_BASE" != "$RUNTIME" ] || RT_BASE="${RUNTIME%%-rc.*}" ;;
esac
case "$RT_BASE" in
    ""|*[!0-9.]*) echo "error: runtime-version '$RUNTIME' is not a dotted numeric version" >&2; exit 1 ;;
esac
case "$RUNTIME" in
    "$RT_BASE") ;;
    "$RT_BASE"-beta.[0-9]*|"$RT_BASE"-rc.[0-9]*) ;;
    *) echo "error: runtime-version '$RUNTIME' has an invalid prerelease" >&2; exit 1 ;;
esac

# deno_runtime is pinned exactly: deno_runtime = { version = "=0.266.0", ...
DENO="$(sed -n 's/.*deno_runtime = { version = "=\([0-9][0-9.]*\)".*/\1/p' \
    "$ROOT/crates/inka-runtime/Cargo.toml" | head -1 || true)"
[ -n "$DENO" ] || { echo "error: could not parse deno_runtime version" >&2; exit 1; }

echo "release=$REL_V"
echo "runtime=$RUNTIME"
echo "deno=$DENO"
echo "channel=$CHANNEL"
echo "base=$BASE"
echo "tag=$TAG"

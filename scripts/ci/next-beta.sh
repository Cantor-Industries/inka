#!/usr/bin/env bash
# Print the next beta counter for a version base.
#
#   scripts/ci/next-beta.sh <base>          toolchain release base (e.g. 0.8.1)
#   scripts/ci/next-beta.sh --runtime       runtime tuple base (from
#                                           crates/inka-runtime/runtime-version)
#
# Toolchain mode reads existing git tags (`v<base>-beta.N[-hash]`) and prints
# `<base>-beta.<max N + 1>` (or `-beta.1` when none exist). Runtime mode reads
# the current runtime tuple and advances its prerelease counter.
#
# Note: the toolchain release counter and the runtime tuple counter are
# independent; a runtime revision is only required when the engine changes.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

usage() {
    echo "usage: next-beta.sh <base> | next-beta.sh --runtime" >&2
    exit 2
}

mode=toolchain
base=""
for arg in "$@"; do
    case "$arg" in
        --runtime) mode=runtime ;;
        -h|--help) usage ;;
        *) [ -z "$base" ] || usage; base="$arg" ;;
    esac
done

# Advance an existing `<base>-beta.N`; a plain `<base>` starts at `.1`.
advance() {
    _cur="$1"
    case "$_cur" in
        *-beta.*)
            _n="${_cur##*-beta.}"
            case "$_n" in
                ""|*[!0-9]*) echo "error: '$1' has an invalid beta counter" >&2; exit 1 ;;
            esac
            printf '%s-beta.%s' "${_cur%%-beta.*}" "$((_n + 1))"
            ;;
        *) printf '%s-beta.1' "$_cur" ;;
    esac
}

if [ "$mode" = runtime ]; then
    rel="$(tr -d '[:space:]' < "$ROOT/crates/inka-runtime/runtime-version")"
    [ -n "$rel" ] || { echo "error: empty runtime-version" >&2; exit 1; }
    advance "$rel"
    exit 0
fi

[ -n "$base" ] || usage
case "$base" in
    *[!0-9.]*|"") echo "error: base must be a dotted numeric version, got '$base'" >&2; exit 1 ;;
esac

# Highest existing `beta.N` for this base across all tags.
max=0
while IFS= read -r tag; do
    [ -n "$tag" ] || continue
    n="${tag#v"$base"-beta.}"
    n="${n%%-*}"
    case "$n" in
        ""|*[!0-9]*) continue ;;
    esac
    [ "$n" -gt "$max" ] && max="$n"
done < <(git -C "$ROOT" tag -l "v$base-beta.*" 2>/dev/null || true)

printf '%s-beta.%s\n' "$base" "$((max + 1))"

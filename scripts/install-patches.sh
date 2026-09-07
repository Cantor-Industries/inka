#!/usr/bin/env bash
# Colocate the default curated patch specs (and optionally the inka-patcher
# binary) with a prefix directory so CJS->ESM conversion works without the
# repo tree. `inka` discovers them by adjacency:
#   <dir of inka binary>/patches   (or $INKA_PATCHES)
#   <dir of inka binary>/inka-patcher  (or $INKA_PATCHER)
#
# usage:
#   scripts/install-patches.sh <prefix-dir> [<path-to-built-inka-patcher>]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ "$#" -lt 1 ]; then
    echo "usage: $0 <prefix-dir> [<path-to-built-inka-patcher>]" >&2
    echo "copies <repo>/patches into <prefix-dir>/patches; optionally copies the patcher as <prefix-dir>/inka-patcher" >&2
    exit 1
fi

PREFIX="$1"
mkdir -p "$PREFIX"

mkdir -p "$PREFIX/patches"
cp -R "$ROOT"/patches/. "$PREFIX/patches/"
echo "installed curated patch specs into $PREFIX/patches"

if [ "$#" -ge 2 ]; then
    if [ ! -f "$2" ]; then
        echo "error: inka-patcher binary not found at $2" >&2
        exit 1
    fi
    cp "$2" "$PREFIX/inka-patcher"
    chmod +x "$PREFIX/inka-patcher"
    echo "installed inka-patcher into $PREFIX/inka-patcher"
fi

echo "done: put the 'inka' binary in $PREFIX (or keep it beside these) to enable CJS conversion"

#!/usr/bin/env bash
# Print "<archive> <sha256>" for a pinned laufey backend archive.
#
# Reads the trust anchor from crates/inka/src/desktop.rs (LAUFEY_VERSION and
# LAUFEY_SUMS) so the release workflow downloads exactly the artifacts inka
# verifies at runtime, without duplicating the checksums.
#
# usage: laufey-asset.sh <backend> <target>
#   backend: cef | webview | raw   (raw ships upstream as `winit`)
#   target:  x86_64-unknown-linux-gnu | ...
set -euo pipefail

backend="${1:?usage: laufey-asset.sh <backend> <target>}"
target="${2:?usage: laufey-asset.sh <backend> <target>}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DESKTOP="$ROOT/crates/inka/src/desktop.rs"
[ -f "$DESKTOP" ] || { echo "error: missing $DESKTOP" >&2; exit 1; }

version="$(sed -n 's/.*const LAUFEY_VERSION: &str = "\([0-9][0-9.]*\)".*/\1/p' "$DESKTOP" | head -1)"
[ -n "$version" ] || { echo "error: could not parse LAUFEY_VERSION" >&2; exit 1; }

archive_backend="$backend"
[ "$backend" = raw ] && archive_backend=winit
ext=tar.gz
case "$target" in *windows*) ext=zip ;; esac
archive="laufey-$archive_backend-$target.$ext"

# The pinned sha is the line after the archive name in LAUFEY_SUMS.
sha="$(grep -A1 -F "\"$archive\"" "$DESKTOP" \
    | sed -n '2s/.*"\([0-9a-fA-F]\{64\}\)".*/\1/p' | head -1)"
[ -n "$sha" ] || { echo "error: no pinned sha256 for $archive in LAUFEY_SUMS" >&2; exit 1; }

printf '%s %s\n' "$archive" "$sha"

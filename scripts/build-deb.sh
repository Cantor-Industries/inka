#!/usr/bin/env bash
# Build a standalone toolchain-only .deb for inka (amd64 by default).
#
# The package installs the inka CLI + launcher + patcher + curated patch specs
# under /usr/lib/inka, with /usr/bin/inka symlinked there, so all exe-adjacent
# discovery (launcher, inka-patcher, patches/) works. It deliberately does NOT
# bundle a runtime or store: those stay per-user under ~/.inka-runtime
# (INKA_RUNTIME_HOME), installed with `inka install <ver> --from <base>`.
#
# Versioning (upgrade-safe / apt-sortable): $INKA_DEB_VERSION overrides, else
# the newest git tag (git describe --tags --abbrev=0), else 0.1.0. Pre-release
# tags must use Debian ordering, e.g. 0.1.0~rc1.
#
# usage:
#   INKA_DEB_VERSION=0.1.0 \
#   DEB_MAINTAINER="Name <email>" \
#   DEB_HOMEPAGE="https://…" \
#   INKA_PATCHER=/path/to/inka-patcher \
#   scripts/build-deb.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="${INKA_DEB_VERSION:-$(git describe --tags --abbrev=0 2>/dev/null || true)}"
VERSION="${VERSION:-0.1.0}"
case "$VERSION" in
    *[!0-9A-Za-z.+~-]*) echo "error: invalid Debian version '$VERSION'" >&2; exit 1 ;;
esac
ARCH="${DEB_ARCH:-amd64}"
MAINTAINER="${DEB_MAINTAINER:-Inka Developers <dev@inka.invalid>}"
HOMEPAGE="${DEB_HOMEPAGE:-https://inka.invalid}"
PATCHER="${INKA_PATCHER:-/media/kook/641ee182-ef10-4fc8-96b8-2de6f780603f/inka-build/target/release/inka-patcher}"

echo "== build toolchain release (inka, inka-launcher, inka-resolver) =="
cargo build --release -p inka -p inka-launcher -p inka-resolver

[ -x target/release/inka ] || { echo "error: target/release/inka missing" >&2; exit 1; }
[ -x target/release/inka-launcher ] || { echo "error: target/release/inka-launcher missing" >&2; exit 1; }
[ -f "$PATCHER" ] || { echo "error: inka-patcher not found at $PATCHER (set INKA_PATCHER)" >&2; exit 1; }
[ -d patches ] || { echo "error: patches/ dir missing" >&2; exit 1; }

STAGE="$(mktemp -d "${TMPDIR:-/tmp}/inka-deb.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT

LIB="$STAGE/usr/lib/inka"
mkdir -p "$LIB" "$STAGE/usr/bin" "$STAGE/usr/share/doc/inka" "$STAGE/DEBIAN"

install -m 0755 target/release/inka "$LIB/inka"
install -m 0755 target/release/inka-launcher "$LIB/inka-launcher"
install -m 0755 "$PATCHER" "$LIB/inka-patcher"
mkdir -p "$LIB/patches"
cp -R patches/. "$LIB/patches/"

# /usr/bin/inka -> /usr/lib/inka/inka (current_exe resolves the real path, so
# adjacency discovery for launcher/patcher/patches keeps working).
ln -s ../lib/inka/inka "$STAGE/usr/bin/inka"

cat > "$STAGE/usr/share/doc/inka/copyright" <<EOF
inka — single-file executables that load a shared Deno runtime

Copyright holders: see the upstream source repository.

This package is distributed under the terms of its upstream license.
See https://${HOMEPAGE#https://} for the source and full license text.
EOF
install -m 0644 README.md "$STAGE/usr/share/doc/inka/README.md"

INSTALLED_KB="$(du -sk "$LIB" | cut -f1)"
cat > "$STAGE/DEBIAN/control" <<EOF
Package: inka
Version: ${VERSION}
Architecture: ${ARCH}
Maintainer: ${MAINTAINER}
Installed-Size: ${INSTALLED_KB}
Depends: libc6
Section: utils
Priority: optional
Homepage: ${HOMEPAGE}
Description: inka toolchain — build/run single-file executables on a shared Deno runtime
 The inka CLI packs a launcher + your source + a manifest into a single
 executable that loads a per-machine Deno runtime tuple (libinka_runtime-<v>.so)
 plus an optional package store. This package ships the toolchain only
 (inka, inka-launcher, inka-patcher, curated patch specs); install a runtime
 per user with: inka install <version> --from <base>.
EOF

( cd "$STAGE" && find . -type f ! -path './DEBIAN/*' -exec md5sum {} \; \
    | sed 's|^\./||' > DEBIAN/md5sums )

OUT="target/inka_${VERSION}_${ARCH}.deb"
dpkg-deb --build "$STAGE" "$OUT" >/dev/null
echo "built $OUT"

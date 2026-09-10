#!/usr/bin/env bash
# Build a standalone toolchain .deb for inka (amd64 by default).
#
# The package installs the inka CLI + launcher + patcher + curated patch specs
# under /usr/lib/inka, with /usr/bin/inka symlinked there, so all exe-adjacent
# discovery (launcher, inka-patcher, patches/) works.
#
# It can also bundle the shared engine so a first `apt install` is fully
# self-contained:
#   INKA_DEB_RUNTIME_SO   path to libinka_runtime-<v>.so (bundled on runtime change)
#   INKA_DEB_RESOLVER_SO  path to libinka_resolver-<v>.so (small; always bundled)
#   INKA_DEB_STORE_TAR    path to store.tar.gz (bundled on store change)
#   INKA_DEB_STORE_MANIFEST path to seed-manifest.json (with the store tar)
#   INKA_DEB_RELEASE_BASE optional channel base baked into the postinst
#
# A postinst (best-effort; never fails dpkg) copies any bundled runtime/resolver
# into /usr/local/lib/inka-runtime and seeds the installing account's XDG store
# from the bundled snapshot; with no bundled runtime it runs `inka update`.
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
# Debian upstream versions must start with a digit; strip a leading "v" from tags.
VERSION="${VERSION#v}"
case "$VERSION" in
    *[!0-9A-Za-z.+~-]*) echo "error: invalid Debian version '$VERSION'" >&2; exit 1 ;;
esac
ARCH="${DEB_ARCH:-amd64}"
MAINTAINER="${DEB_MAINTAINER:-Inka Developers <dev@inka.invalid>}"
HOMEPAGE="${DEB_HOMEPAGE:-https://inka.invalid}"
PATCHER="${INKA_PATCHER:-/media/kook/641ee182-ef10-4fc8-96b8-2de6f780603f/inka-build/target/release/inka-patcher}"

# Binaries come from the cargo release dir (CARGO_TARGET_DIR when set, e.g. the
# CI runner; else the repo-local target/release).
if [ -n "${CARGO_TARGET_DIR:-}" ]; then
    BIN_DIR="$CARGO_TARGET_DIR/release"
else
    BIN_DIR="$ROOT/target/release"
fi

echo "== build toolchain release (inka, inka-launcher, inka-resolver) =="
if [ ! -x "$BIN_DIR/inka" ] || [ ! -x "$BIN_DIR/inka-launcher" ]; then
    cargo build --release -p inka -p inka-launcher -p inka-resolver
fi

[ -x "$BIN_DIR/inka" ] || { echo "error: $BIN_DIR/inka missing" >&2; exit 1; }
[ -x "$BIN_DIR/inka-launcher" ] || { echo "error: $BIN_DIR/inka-launcher missing" >&2; exit 1; }
[ -f "$PATCHER" ] || { echo "error: inka-patcher not found at $PATCHER (set INKA_PATCHER)" >&2; exit 1; }
[ -d "$ROOT/patches" ] || { echo "error: patches/ dir missing" >&2; exit 1; }

STAGE="$(mktemp -d "${TMPDIR:-/tmp}/inka-deb.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT

LIB="$STAGE/usr/lib/inka"
mkdir -p "$LIB" "$STAGE/usr/bin" "$STAGE/usr/share/doc/inka" "$STAGE/DEBIAN"

install -m 0755 "$BIN_DIR/inka" "$LIB/inka"
install -m 0755 "$BIN_DIR/inka-launcher" "$LIB/inka-launcher"
install -m 0755 "$PATCHER" "$LIB/inka-patcher"
mkdir -p "$LIB/patches"
cp -R "$ROOT/patches/." "$LIB/patches/"

# Optional engine payloads (self-contained first install).
if [ -n "${INKA_DEB_RESOLVER_SO:-}" ]; then
    [ -f "$INKA_DEB_RESOLVER_SO" ] || { echo "error: INKA_DEB_RESOLVER_SO missing: $INKA_DEB_RESOLVER_SO" >&2; exit 1; }
    install -m 0755 "$INKA_DEB_RESOLVER_SO" "$LIB/$(basename "$INKA_DEB_RESOLVER_SO")"
fi
if [ -n "${INKA_DEB_RUNTIME_SO:-}" ]; then
    [ -f "$INKA_DEB_RUNTIME_SO" ] || { echo "error: INKA_DEB_RUNTIME_SO missing: $INKA_DEB_RUNTIME_SO" >&2; exit 1; }
    install -m 0755 "$INKA_DEB_RUNTIME_SO" "$LIB/$(basename "$INKA_DEB_RUNTIME_SO")"
fi
if [ -n "${INKA_DEB_STORE_TAR:-}" ]; then
    [ -f "$INKA_DEB_STORE_TAR" ] || { echo "error: INKA_DEB_STORE_TAR missing: $INKA_DEB_STORE_TAR" >&2; exit 1; }
    install -m 0644 "$INKA_DEB_STORE_TAR" "$LIB/store.tar.gz"
    if [ -n "${INKA_DEB_STORE_MANIFEST:-}" ]; then
        install -m 0644 "$INKA_DEB_STORE_MANIFEST" "$LIB/seed-manifest.json"
    fi
fi

# /usr/bin/inka -> /usr/lib/inka/inka (current_exe resolves the real path, so
# adjacency discovery for launcher/patcher/patches keeps working).
ln -s ../lib/inka/inka "$STAGE/usr/bin/inka"

cat > "$STAGE/usr/share/doc/inka/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: inka

Files: *
Copyright: 2026 Cantor Industries Authors
License: MIT

License: MIT
 Permission is hereby granted, free of charge, to any person obtaining a copy
 of this software and associated documentation files (the "Software"), to deal
 in the Software without restriction, including without limitation the rights
 to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 copies of the Software, and to permit persons to whom the Software is
 furnished to do so, subject to the following conditions:
 .
 The above copyright notice and this permission notice shall be included in all
 copies or substantial portions of the Software.
 .
 THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 SOFTWARE.

Third-party code linked into the binaries retains its own licenses and copyrights
(e.g. the Deno authors for the embedded runtime; see the upstream repository).
EOF
install -m 0644 "$ROOT/README.md" "$STAGE/usr/share/doc/inka/README.md"

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
 plus a package store. This package ships the toolchain (inka, inka-launcher,
 inka-patcher, curated patch specs) and may bundle the shared runtime/resolver
 and a seeded package store so a first install is self-contained. Run
 'inka update' to fetch the newest runtime, resolver, and store.
EOF

# postinst: best-effort runtime/resolver install + store seed; never fails dpkg.
RELEASE_BASE_DEFAULT="${INKA_DEB_RELEASE_BASE:-https://github.com/Cantor-Industries/inka/releases/latest/download}"
cat > "$STAGE/DEBIAN/postinst" <<'POSTINST'
#!/bin/sh
# Best-effort runtime + store provisioning. Always exits 0 so dpkg never fails.
set -u

LIB=/usr/lib/inka
RUNTIME_DIR=/usr/local/lib/inka-runtime
RELEASE_BASE_DEFAULT="__INKA_RELEASE_BASE__"
log() { echo "[inka] $*" >&2; }

# 1) copy any bundled runtime/resolver into the machine-wide runtime dir
mkdir -p "$RUNTIME_DIR" 2>/dev/null || true
for f in "$LIB"/libinka_runtime-*.so "$LIB"/libinka_resolver-*.so; do
    [ -e "$f" ] || continue
    base=$(basename "$f")
    if [ ! -e "$RUNTIME_DIR/$base" ] || ! cmp -s "$f" "$RUNTIME_DIR/$base"; then
        if cp -f "$f" "$RUNTIME_DIR/$base" 2>/dev/null; then
            chmod 0755 "$RUNTIME_DIR/$base" 2>/dev/null || true
            log "installed $base into $RUNTIME_DIR"
        fi
    fi
done

# 2) seed the installing account's XDG store from the bundled snapshot
if [ -f "$LIB/store.tar.gz" ] && [ -f "$LIB/seed-manifest.json" ]; then
    user="${SUDO_USER:-root}"
    if [ -z "$user" ] || [ "$user" = "root" ]; then
        home="${HOME:-/root}"
        group="$(id -gn root 2>/dev/null || echo root)"
        user="root"
    else
        home="$(getent passwd "$user" 2>/dev/null | cut -d: -f6)"
        group="$(id -gn "$user" 2>/dev/null || echo "$user")"
    fi
    if [ -n "$home" ] && [ -d "$home" ]; then
        data="${XDG_DATA_HOME:-$home/.local/share}/inka"
        store="$data/store"
        want=$(sed -n 's/.*"sha256"[^"]*"\([^"]*\)".*/\1/p' "$LIB/seed-manifest.json" | head -1)
        have=""
        [ -f "$store/seed-manifest.json" ] && \
            have=$(sed -n 's/.*"sha256"[^"]*"\([^"]*\)".*/\1/p' "$store/seed-manifest.json" | head -1)
        if [ -n "$want" ] && [ "$want" != "$have" ]; then
            mkdir -p "$store" 2>/dev/null || true
            rm -rf "$store/node_modules" 2>/dev/null || true
            if tar -xzf "$LIB/store.tar.gz" -C "$store" 2>/dev/null; then
                cp -f "$LIB/seed-manifest.json" "$store/seed-manifest.json" 2>/dev/null || true
                chown -R "$user:$group" "$data" 2>/dev/null || true
                log "seeded package store at $store"
            fi
        fi
    fi
fi

# 3) no runtime present and none bundled? best-effort online fetch.
if ! ls "$RUNTIME_DIR"/libinka_runtime-*.so >/dev/null 2>&1; then
    base="${INKA_RELEASE_BASE:-$RELEASE_BASE_DEFAULT}"
    if INKA_RELEASE_BASE="$base" "$LIB/inka" update --home "$RUNTIME_DIR" >/dev/null 2>&1; then
        log "downloaded runtime into $RUNTIME_DIR"
    else
        log "no runtime installed yet; run 'inka update' (or 'sudo inka update') when online"
    fi
fi

exit 0
POSTINST
chmod 0755 "$STAGE/DEBIAN/postinst"
# bake an override channel base, if given
if [ -n "${INKA_DEB_RELEASE_BASE:-}" ]; then
    sed -i "s|__INKA_RELEASE_BASE__|${INKA_DEB_RELEASE_BASE}|" "$STAGE/DEBIAN/postinst"
else
    sed -i "s|__INKA_RELEASE_BASE__|${RELEASE_BASE_DEFAULT}|" "$STAGE/DEBIAN/postinst"
fi

( cd "$STAGE" && find . -type f ! -path './DEBIAN/*' -exec md5sum {} \; \
    | sed 's|^\./||' > DEBIAN/md5sums )

OUT="${DEB_OUT:-$ROOT/target/inka_${VERSION}_${ARCH}.deb}"
mkdir -p "$(dirname "$OUT")"
dpkg-deb --build "$STAGE" "$OUT" >/dev/null
echo "built $OUT"

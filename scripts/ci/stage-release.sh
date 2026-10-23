#!/usr/bin/env bash
# Stage a Linux release tree for smoke-testing (and for CI to publish).
#
# usage: stage-release.sh <stage-dir>
#
# Assumes the release binaries are already built in $CARGO_TARGET_DIR/release:
# `inka`, `inka-launcher`, `libinka_desktop_shim.so` and `libinka_runtime.so`
# (the runtime with the `desktop` feature).
#
# env:
#   REL        toolchain version (required), e.g. 0.8.2 or 0.8.2-beta.1
#   RUNTIME    runtime tuple     (default: crates/inka-runtime/runtime-version)
#   DENO       deno_runtime base (default: parsed from crates/inka-runtime/Cargo.toml)
#   CHANNEL    stable|beta       (default: derived from REL)
#   BASE       target release    (default: REL without a prerelease suffix)
#   TAG        release tag       (default: $GITHUB_REF_NAME, else v$REL)
#   TARGET     target triple     (default: x86_64-unknown-linux-gnu)
#   NOTES_FILE notes template    (default: RELEASE_NOTES[_BETA].md by channel)
#   SKIP_CEF=1 skip mirroring the pinned CEF archive (partial stage; smoke then
#              skips the desktop-installer step)
#
# Produces what scripts/ci/smoke.sh expects:
#   inka, install.sh, versions.json, RELEASE_NOTES.md,
#   inka-toolchain-<rel>-<target>.tar.gz (+ .sha256),
#   libinka_runtime-<runtime>.so (+ .sha256),
#   laufey-<backend>-<target>.tar.gz (+ .sha256).
set -euo pipefail

usage() {
    echo "usage: stage-release.sh <stage-dir>" >&2
    exit 2
}

STAGE="${1:-}"
[ -n "$STAGE" ] || usage

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/release"

REL="${REL:-}"
[ -n "$REL" ] || { echo "error: REL is required (e.g. REL=0.8.2 $0 <stage-dir>)" >&2; exit 1; }

RUNTIME="${RUNTIME:-$(tr -d '[:space:]' < "$ROOT/crates/inka-runtime/runtime-version")}"
DENO="${DENO:-$(sed -n 's/.*deno_runtime = { version = "=\([0-9][0-9.]*\)".*/\1/p' \
    "$ROOT/crates/inka-runtime/Cargo.toml" | head -1)}"
TARGET="${TARGET:-x86_64-unknown-linux-gnu}"
TAG="${TAG:-${GITHUB_REF_NAME:-v$REL}}"

# Validate everything that is interpolated into sed/JSON. version.sh applies the
# same charset to REL; keep the local path equally strict.
validate() { # <value> <label> <charset-case-pattern>
    case "$1" in
        "" | *[!0-9A-Za-z.+~_-]*)
            echo "error: $2 '$1' contains characters outside the release charset" >&2
            exit 1
            ;;
    esac
}
validate "$REL" REL
validate "$RUNTIME" RUNTIME
validate "$DENO" DENO
validate "$TAG" TAG

CHANNEL="${CHANNEL:-}"
if [ -z "$CHANNEL" ]; then
    case "$REL" in
        *-beta* | *-rc*) CHANNEL=beta ;;
        *) CHANNEL=stable ;;
    esac
fi
case "$CHANNEL" in
    stable | beta) ;;
    *) echo "error: CHANNEL must be stable or beta, got '$CHANNEL'" >&2; exit 1 ;;
esac

BASE="${BASE:-}"
if [ -z "$BASE" ]; then
    if [ "$CHANNEL" = beta ]; then
        BASE="${REL%%-beta.*}"
        [ "$BASE" != "$REL" ] || BASE="${REL%%-rc.*}"
    else
        BASE="$REL"
    fi
fi
validate "$BASE" BASE

NOTES_FILE="${NOTES_FILE:-}"
if [ -z "$NOTES_FILE" ]; then
    NOTES_FILE="$ROOT/RELEASE_NOTES.md"
    [ "$CHANNEL" = beta ] && NOTES_FILE="$ROOT/RELEASE_NOTES_BETA.md"
fi

for f in inka inka-launcher libinka_desktop_shim.so libinka_runtime.so; do
    [ -f "$BIN/$f" ] || {
        echo "error: missing $BIN/$f (build the release artifacts first)" >&2
        exit 1
    }
done
[ -f "$NOTES_FILE" ] || { echo "error: missing notes template $NOTES_FILE" >&2; exit 1; }
[ -f "$ROOT/install.sh" ] || { echo "error: missing $ROOT/install.sh" >&2; exit 1; }

mkdir -p "$STAGE"
STAGE="$(cd "$STAGE" && pwd)"
TC="inka-toolchain-${REL}-${TARGET}.tar.gz"

# Toolchain binaries, packaged as one archive.
cp "$BIN/inka" "$STAGE/inka"
cp "$BIN/inka-launcher" "$STAGE/inka-launcher"
cp "$BIN/libinka_desktop_shim.so" "$STAGE/libinka_desktop_shim.so"
chmod +x "$STAGE/inka" "$STAGE/inka-launcher" "$STAGE/libinka_desktop_shim.so"
tar -czf "$STAGE/$TC" -C "$STAGE" inka inka-launcher libinka_desktop_shim.so

# Engine asset (always published; consumers keep only newer tuples).
cp "$BIN/libinka_runtime.so" "$STAGE/libinka_runtime-$RUNTIME.so"

# Shared CEF runtime: mirror the pinned laufey archive so
# `inka desktop --installer` can provision it without reaching laufey's host,
# verified against the same inka trust anchor used at run time.
LAUFEY_VERSION="$(sed -n 's/.*const LAUFEY_VERSION: &str = "\([0-9][0-9.]*\)".*/\1/p' \
    "$ROOT/crates/inka/src/desktop.rs" | head -1)"
CEF_ASSET="$(scripts/ci/laufey-asset.sh cef "$TARGET")"
CEF_ARCHIVE="${CEF_ASSET%% *}"
CEF_SHA="${CEF_ASSET##* }"
if [ "${SKIP_CEF:-0}" = "1" ]; then
    echo "skip: CEF archive (SKIP_CEF=1)"
else
    curl --proto '=https' --tlsv1.2 -fsSL \
        "https://github.com/littledivy/laufey/releases/download/v$LAUFEY_VERSION/$CEF_ARCHIVE" \
        -o "$STAGE/$CEF_ARCHIVE"
    GOT="$(sha256sum "$STAGE/$CEF_ARCHIVE" | awk '{print $1}')"
    [ "$GOT" = "$CEF_SHA" ] \
        || { echo "error: checksum mismatch for $CEF_ARCHIVE (pinned $CEF_SHA, got $GOT)" >&2; exit 1; }
    printf '%s\n' "$GOT" > "$STAGE/$CEF_ARCHIVE.sha256"
fi

# The bootstrap script and the curated release notes. The notes carry
# {{REL}}/{{RUNTIME}}/{{TAG}}/{{CHANNEL}} placeholders and a leading maintainer
# comment block that is stripped from the published body.
cp "$ROOT/install.sh" "$STAGE/install.sh"
sed -e "s/{{REL}}/$REL/g" -e "s/{{RUNTIME}}/$RUNTIME/g" \
    -e "s/{{TAG}}/$TAG/g" -e "s/{{CHANNEL}}/$CHANNEL/g" \
    "$NOTES_FILE" \
    | sed '1,/^-->$/d' > "$STAGE/RELEASE_NOTES.md"

# sha256 sidecars (bare hex, as the installer/update expect).
for f in "$TC" "libinka_runtime-$RUNTIME.so"; do
    (cd "$STAGE" && sha256sum "$f" | awk '{print $1}' > "$f.sha256")
done

# Machine-readable version/identity record.
RT_SHA="$(cat "$STAGE/libinka_runtime-$RUNTIME.so.sha256")"
TC_SHA="$(cat "$STAGE/$TC.sha256")"
cat > "$STAGE/versions.json" <<EOF
{
  "release": "$REL",
  "channel": "$CHANNEL",
  "base": "$BASE",
  "tag": "$TAG",
  "toolchain": {
    "version": "$REL",
    "target": "$TARGET",
    "archive": "$TC",
    "sha256": "$TC_SHA"
  },
  "runtime": "$RUNTIME",
  "deno_runtime": "$DENO",
  "runtime_sha256": "$RT_SHA",
  "laufey": {
    "version": "$LAUFEY_VERSION",
    "target": "$TARGET",
    "backends": {
      "cef": {
        "archive": "$CEF_ARCHIVE",
        "sha256": "$CEF_SHA"
      }
    }
  }
}
EOF

echo "staged $REL (runtime $RUNTIME, deno $DENO, channel $CHANNEL, tag $TAG)"
ls -1 "$STAGE"
echo "next: scripts/ci/smoke.sh \"$STAGE\""

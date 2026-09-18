#!/usr/bin/env bash
# Dev spike: verify the launcher picks the right runtime tuple (roll-forward vs
# tested-against cap) using the ABI-compatible stub runtime.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

RTDIR="${INKA_RUNTIME_HOME:-$HOME/.inka-runtime}"
export INKA_RUNTIME_HOME="$RTDIR"
mkdir -p "$RTDIR"

# Honor CARGO_TARGET_DIR (the big-disk target); default to the repo target.
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="$TARGET_DIR/release"

echo "== build tooling (launcher, stub 0.0.0, inka --features bundle) =="
# The stub embeds INKA_STUB_VERSION at compile time (build.rs tracks it), so pin
# it explicitly for the 0.0.0 build too.
INKA_STUB_VERSION=0.0.0 cargo build --release -p inka-launcher -p inka-runtime-stub
cargo build --release -p inka --features bundle

echo "== install runtime tuples =="
cp "$BIN/libinka_runtime_stub.so" "$RTDIR/libinka_runtime-0.0.0.so"
INKA_STUB_VERSION=0.1.0 cargo build --release -p inka-runtime-stub
cp "$BIN/libinka_runtime_stub.so" "$RTDIR/libinka_runtime-0.1.0.so"
# A prerelease tuple, to exercise beta-channel gating in the launcher.
INKA_STUB_VERSION=0.2.0-beta.1 cargo build --release -p inka-runtime-stub
cp "$BIN/libinka_runtime_stub.so" "$RTDIR/libinka_runtime-0.2.0-beta.1.so"

echo "== pack demo artifacts (inka build) =="
# Cap both artifacts below any real installed runtime so the launcher is forced
# to choose between the stub tuples (0.0.0 / 0.1.0) deterministically.
"$BIN/inka" build \
  --source demo/main.js \
  --runtime '>=0.0.0' \
  --tested-against 0.1.0 \
  -o demo/hello
"$BIN/inka" build \
  --source demo/main.js \
  --runtime '>=0.0.0' \
  --tested-against 0.0.0 \
  -o demo/hello-pinned
# A cap that admits the beta tuple: stable must skip it, `--beta` must use it.
"$BIN/inka" build \
  --source demo/main.js \
  --runtime '>=0.0.0' \
  --tested-against 0.2.0 \
  -o demo/hello-cap
"$BIN/inka" build --beta \
  --source demo/main.js \
  --runtime '>=0.0.0' \
  --tested-against 0.2.0 \
  -o demo/hello-beta
chmod +x demo/hello demo/hello-pinned demo/hello-cap demo/hello-beta

expect_runtime() { # <exe> <version> <label>
    local out
    out="$(INKA_DEBUG=1 INKA_STUB_ECHO=1 "$1" hello arg1 arg2 2>&1)"
    case "$out" in
        *"resolved inka_runtime $2 "*) echo "ok: $3 picked $2" ;;
        *)
            echo "FAIL: $3 expected runtime $2" >&2
            printf '%s\n' "$out" >&2
            exit 1
            ;;
    esac
}

echo
echo "== roll-forward (>=0.0.0, capped 0.1.0) -> 0.1.0 =="
expect_runtime ./demo/hello 0.1.0 "roll-forward"
echo "== pinned (tested-against 0.0.0) -> 0.0.0 =="
expect_runtime ./demo/hello-pinned 0.0.0 "pinned"
echo
echo "== stable artifact skips the beta tuple -> 0.1.0 =="
expect_runtime ./demo/hello-cap 0.1.0 "stable skips beta"
echo "== --beta artifact selects the prerelease -> 0.2.0-beta.1 =="
expect_runtime ./demo/hello-beta 0.2.0-beta.1 "beta channel"

echo
echo "== sizes =="
ls -l "$BIN/inka" "$BIN/inka-launcher" "$RTDIR"/libinka_runtime-*.so \
    demo/hello demo/hello-pinned demo/hello-cap demo/hello-beta | awk '{print $5"\t"$9}'

echo "spike: OK"

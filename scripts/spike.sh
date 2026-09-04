#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

RTDIR="${INKA_RUNTIME_HOME:-$HOME/.inka-runtime}"
mkdir -p "$RTDIR"

echo "== build tooling (launcher, stub 0.0.0, inka) =="
cargo build --release -p launcher -p runtime-stub -p inka

echo "== install runtime tuples =="
cp target/release/libinka_runtime_stub.so "$RTDIR/libinka_runtime-0.0.0.so"
INKA_STUB_VERSION=0.1.0 cargo build --release -p runtime-stub
cp target/release/libinka_runtime_stub.so "$RTDIR/libinka_runtime-0.1.0.so"

echo "== pack demo artifacts (inka build) =="
./target/release/inka build \
  --source demo/main.js \
  --manifest demo/hello.manifest \
  -o demo/hello
./target/release/inka build \
  --source demo/main.js \
  --manifest demo/hello-pinned.manifest \
  -o demo/hello-pinned
chmod +x demo/hello demo/hello-pinned

echo
echo "== run: roll-forward (manifest >=0.0.0, no pin) -> should pick 0.1.0 =="
INKA_STUB_ECHO=1 ./demo/hello hello arg1 arg2
echo
echo "== run: pinned (tested-against 0.0.0) -> should pick 0.0.0 =="
INKA_STUB_ECHO=1 ./demo/hello-pinned
echo
echo "== sizes =="
ls -l target/release/inka target/release/inka-launcher "$RTDIR"/libinka_runtime-*.so demo/hello demo/hello-pinned | awk '{print $5"\t"$9}'

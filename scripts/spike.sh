#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

RTDIR="${INKA_RUNTIME_HOME:-$HOME/.inka-runtime}"
export INKA_RUNTIME_HOME="$RTDIR"
mkdir -p "$RTDIR"

echo "== build tooling (launcher, stub 0.0.0, inka) =="
cargo build --release -p inka-launcher -p inka-runtime-stub -p inka

echo "== install runtime tuples =="
cp target/release/libinka_runtime_stub.so "$RTDIR/libinka_runtime-0.0.0.so"
INKA_STUB_VERSION=0.1.0 cargo build --release -p inka-runtime-stub
cp target/release/libinka_runtime_stub.so "$RTDIR/libinka_runtime-0.1.0.so"

echo "== pack demo artifacts (inka build) =="
# Cap both artifacts below any real installed runtime so the launcher is forced
# to choose between the stub tuples (0.0.0 / 0.1.0) deterministically.
./target/release/inka build \
  --source demo/main.js \
  --runtime '>=0.0.0' \
  --tested-against 0.1.0 \
  -o demo/hello
./target/release/inka build \
  --source demo/main.js \
  --runtime '>=0.0.0' \
  --tested-against 0.0.0 \
  -o demo/hello-pinned
chmod +x demo/hello demo/hello-pinned

echo
echo "== run: roll-forward (runtime >=0.0.0, capped 0.1.0) -> should pick 0.1.0 =="
INKA_STUB_ECHO=1 ./demo/hello hello arg1 arg2
echo
echo "== run: pinned (tested-against 0.0.0) -> should pick 0.0.0 =="
INKA_STUB_ECHO=1 ./demo/hello-pinned
echo
echo "== sizes =="
ls -l target/release/inka target/release/inka-launcher "$RTDIR"/libinka_runtime-*.so demo/hello demo/hello-pinned | awk '{print $5"\t"$9}'

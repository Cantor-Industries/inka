#!/usr/bin/env bash
# Runtime CJS/ESM contract matrix.
#
# Exercises the engine's node-services seam against a throwaway store. Run it
# after any `deno_runtime` tuple bump or change to crates/inka-runtime.
#
# usage: runtime-matrix.sh [path-to-inka] [path-to-patched-store]
#
# The patched-store checks (effect/hono/ws/@std/assert/node:vm, require(esm))
# are skipped when no patched store is available.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
INKA="${1:-$ROOT/target/debug/inka}"
PATCHED_STORE="${2:-${INKA_STORE:-$HOME/.local/share/inka/store}}"

[ -x "$INKA" ] || { echo "error: inka binary not found: $INKA" >&2; exit 1; }

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/inka-matrix.XXXXXX")"
trap 'rm -rf "$SCRATCH"' EXIT

STORE="$SCRATCH/store"
mkdir -p "$STORE"

echo "== building throwaway store (ms, ws, debug) =="
(cd "$STORE" && npm install --no-save --omit=dev ms@2.1.3 ws@8.21.3 debug@4.3.7 >/dev/null 2>&1)

# An unpatched CJS fixture with __esModule and a circular pair.
mkdir -p "$STORE/node_modules/esmflag" "$STORE/node_modules/circ"
printf '%s\n' '{"name":"esmflag","version":"1.0.0","main":"index.js"}' > "$STORE/node_modules/esmflag/package.json"
printf '%s\n' 'exports.__esModule = true;' 'exports.default = 5;' 'exports.foo = "foo";' > "$STORE/node_modules/esmflag/index.js"
printf '%s\n' '{"name":"circ","version":"1.0.0","main":"a.js"}' > "$STORE/node_modules/circ/package.json"
printf '%s\n' 'exports.name = "a";' 'const b = require("./b.js");' 'exports.bName = b.name;' > "$STORE/node_modules/circ/a.js"
printf '%s\n' 'exports.name = "b";' 'const a = require("./a.js");' 'exports.aName = a.name;' > "$STORE/node_modules/circ/b.js"

cd "$SCRATCH"

run() { # <name> <expected-substring> <store> <file> <source>
    local name="$1" want="$2" store="$3" file="$4" src="$5"
    printf '%s\n' "$src" > "$file"
    local out
    out="$(INKA_STORE="$store" "$INKA" run -A "$file" 2>&1)" || {
        echo "FAIL: $name" >&2; printf '%s\n' "$out" >&2; exit 1
    }
    case "$out" in
        *"$want"*) echo "ok: $name" ;;
        *) echo "FAIL: $name (want '$want')" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
    esac
}

echo "== CJS require =="
run "require leaf (ms)" "cjs-ms function 1m" "$STORE" p_ms.js \
'import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const ms = require("ms");
console.log("cjs-ms", typeof ms, ms(60000));'

run "require wrapper (ws)" "cjs-ws function function" "$STORE" p_ws.js \
'import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const ws = require("ws");
console.log("cjs-ws", typeof ws.WebSocket, typeof ws.WebSocketServer);'

run "require nested dep (debug)" "cjs-debug function" "$STORE" p_debug.js \
'import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
console.log("cjs-debug", typeof require("debug"));'

run "require builtin" "cjs-builtin a/b" "$STORE" p_builtin.js \
'import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
console.log("cjs-builtin", require("node:path").join("a", "b"));'

echo "== ESM import of CJS =="
run "esm named import (ws)" "esm-ws function" "$STORE" p_esmws.js \
'import { WebSocket } from "ws";
console.log("esm-ws", typeof WebSocket);'

run "esm default+named (debug)" "mixed-cjs function function" "$STORE" p_esmdebug.js \
'import debug from "debug";
import { WebSocket } from "ws";
console.log("mixed-cjs", typeof debug("x"), typeof WebSocket);'

run "esm __esModule" "esmodule" "$STORE" p_esmflag.js \
'import mod, { foo } from "esmflag";
console.log("esmodule", typeof mod, foo);'

run "circular require" "circular a b" "$STORE" p_circ.js \
'import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const a = require("circ");
console.log("circular", a.name, a.bName);'

if [ -d "$PATCHED_STORE/node_modules/effect" ]; then
    echo "== patched-store ESM matrix =="
    run "patched esm matrix" "esm-matrix function" "$PATCHED_STORE" p_matrix.js \
'import { Effect } from "effect";
import { Hono } from "hono";
import { WebSocket } from "ws";
import { assertEquals } from "@std/assert";
import vm from "node:vm";
console.log("esm-matrix", typeof Effect.succeed, new Hono().routes.length, typeof WebSocket, typeof assertEquals, typeof vm.Script);'

    run "require(esm) effect" "require-esm object function" "$PATCHED_STORE" p_reqesm.js \
'import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const effect = require("effect");
console.log("require-esm", typeof effect.Effect, typeof effect.Effect.succeed);'
else
    echo "skip: patched-store checks (no store at $PATCHED_STORE)"
fi

echo "runtime-matrix: OK"

#!/usr/bin/env bash
# Runtime CJS/ESM contract matrix.
#
# Exercises the engine's node-services seam against a throwaway project whose
# `node_modules` is populated with npm. Run it after any `deno_runtime` tuple
# bump or change to crates/inka-runtime.
#
# usage: runtime-matrix.sh [path-to-inka]
#
# jsr (`@std/assert`), release-package, and native-addon checks are deferred
# until the runtime gains the Deno resolver (C2b).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
INKA="${1:-$ROOT/target/debug/inka}"

[ -x "$INKA" ] || { echo "error: inka binary not found: $INKA" >&2; exit 1; }

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/inka-matrix.XXXXXX")"
trap 'rm -rf "$SCRATCH"' EXIT

echo "== installing packages into the project node_modules =="
(cd "$SCRATCH" && npm install --no-save --omit=dev \
    ms@2.1.3 ws@8.21.3 debug@4.3.7 effect hono @parcel/watcher >/dev/null 2>&1)

# An unpatched CJS fixture with __esModule and a circular pair.
mkdir -p "$SCRATCH/node_modules/esmflag" "$SCRATCH/node_modules/circ"
printf '%s\n' '{"name":"esmflag","version":"1.0.0","main":"index.js"}' > "$SCRATCH/node_modules/esmflag/package.json"
printf '%s\n' 'exports.__esModule = true;' 'exports.default = 5;' 'exports.foo = "foo";' > "$SCRATCH/node_modules/esmflag/index.js"
printf '%s\n' '{"name":"circ","version":"1.0.0","main":"a.js"}' > "$SCRATCH/node_modules/circ/package.json"
printf '%s\n' 'exports.name = "a";' 'const b = require("./b.js");' 'exports.bName = b.name;' > "$SCRATCH/node_modules/circ/a.js"
printf '%s\n' 'exports.name = "b";' 'const a = require("./a.js");' 'exports.aName = a.name;' > "$SCRATCH/node_modules/circ/b.js"

cd "$SCRATCH"

run_with() { # <flags> <name> <expected-substring> <file> <source>
    local flags="$1" name="$2" want="$3" file="$4" src="$5"
    printf '%s\n' "$src" > "$file"
    local out
    # $flags is a deliberate, space-separated flag list.
    # shellcheck disable=SC2086
    out="$("$INKA" run $flags "$file" 2>&1)" || {
        echo "FAIL: $name" >&2; printf '%s\n' "$out" >&2; exit 1
    }
    case "$out" in
        *"$want"*) echo "ok: $name" ;;
        *) echo "FAIL: $name (want '$want')" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
    esac
}

run() { # <name> <expected-substring> <file> <source>
    run_with "-A" "$@"
}

echo "== CJS require =="
run "require leaf (ms)" "cjs-ms function 1m" p_ms.js \
'import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const ms = require("ms");
console.log("cjs-ms", typeof ms, ms(60000));'

run "require wrapper (ws)" "cjs-ws function function" p_ws.js \
'import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const ws = require("ws");
console.log("cjs-ws", typeof ws.WebSocket, typeof ws.WebSocketServer);'

run "require nested dep (debug)" "cjs-debug function" p_debug.js \
'import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
console.log("cjs-debug", typeof require("debug"));'

run "require builtin" "cjs-builtin a/b" p_builtin.js \
'import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
console.log("cjs-builtin", require("node:path").join("a", "b"));'

echo "== ESM import of CJS =="
run "esm named import (ws)" "esm-ws function" p_esmws.js \
'import { WebSocket } from "ws";
console.log("esm-ws", typeof WebSocket);'

run "esm default+named (debug)" "mixed-cjs function function" p_esmdebug.js \
'import debug from "debug";
import { WebSocket } from "ws";
console.log("mixed-cjs", typeof debug("x"), typeof WebSocket);'

run "esm __esModule" "esmodule" p_esmflag.js \
'import mod, { foo } from "esmflag";
console.log("esmodule", typeof mod, foo);'

run "circular require" "circular a b" p_circ.js \
'import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const a = require("circ");
console.log("circular", a.name, a.bName);'

echo "== require permission enforcement =="
SECRET_DIR="$(mktemp -d "${TMPDIR:-/tmp}/inka-secret.XXXXXX")"
SECRET_FILE="$SECRET_DIR/secret.cjs"
printf 'module.exports = 42;\n' > "$SECRET_FILE"
printf '%s\n' \
    'import { createRequire } from "node:module";' \
    'const require = createRequire(import.meta.url);' \
    "try { console.log(\"perm-allowed\", require(\"$SECRET_FILE\")); }" \
    'catch { console.log("perm-denied"); }' > p_perm.js
out="$("$INKA" run p_perm.js 2>&1)" || {
    echo "FAIL: require outside the project errored unexpectedly" >&2; printf '%s\n' "$out" >&2; exit 1
}
case "$out" in
    *perm-denied*) echo "ok: require outside the project denied by default" ;;
    *) echo "FAIL: require outside the project was not denied" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
esac
out="$("$INKA" run -R="$SECRET_DIR" p_perm.js 2>&1)" || {
    echo "FAIL: require with -R errored" >&2; printf '%s\n' "$out" >&2; exit 1
}
case "$out" in
    *"perm-allowed 42"*) echo "ok: require outside the project allowed with -R" ;;
    *) echo "FAIL: require with -R was not allowed" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
esac
rm -rf "$SECRET_DIR"

echo "== jsr / import map (offline Deno cache) =="
mkdir -p jsrproj
printf '%s\n' '{"imports":{"@std/assert":"jsr:@std/assert@1"}}' > jsrproj/deno.json
printf '%s\n' \
    'import { assertEquals } from "@std/assert";' \
    'assertEquals(1, 1);' \
    'console.log("jsr-ok", typeof assertEquals);' > jsrproj/main.ts
if out="$(cd jsrproj && DENO_DIR="${DENO_DIR:-$HOME/.cache/deno}" "$INKA" run main.ts 2>&1)"; then
    case "$out" in
        *jsr-ok*) echo "ok: jsr import map (offline cache)" ;;
        *) echo "skip: jsr import map ($out)" ;;
    esac
else
    echo "skip: jsr import map (no cached @std/assert in ${DENO_DIR:-$HOME/.cache/deno})"
fi

echo "== release packages + native addon (project node_modules) =="
if [ -d "$SCRATCH/node_modules/effect" ] && [ -d "$SCRATCH/node_modules/hono" ]; then
    run "release esm matrix" "esm-matrix function" p_matrix.js \
'import { Effect } from "effect";
import { Hono } from "hono";
import { WebSocket } from "ws";
import vm from "node:vm";
console.log("esm-matrix", typeof Effect.succeed, new Hono().routes.length, typeof WebSocket, typeof vm.Script);'

    run "require(esm) effect" "require-esm object function" p_reqesm.js \
'import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const effect = require("effect");
console.log("require-esm", typeof effect.Effect, typeof effect.Effect.succeed);'
else
    echo "skip: release-package checks (npm install unavailable)"
fi

if [ -f "$SCRATCH/node_modules/@parcel/watcher-linux-x64-glibc/watcher.node" ]; then
    run_with "--allow-sys" "native addon denied without ffi" "native-denied NotCapable" \
        p_native_deny.js \
'import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
try { require("@parcel/watcher"); console.log("native-loaded"); }
catch (e) { console.log("native-denied", e && e.constructor && e.constructor.name); }'

    run_with "--allow-sys --allow-ffi" "native addon loads with ffi" \
        "native-ok function function" p_native.js \
'import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const w = require("@parcel/watcher");
console.log("native-ok", typeof w.subscribe, typeof w.getEventsSince);'
else
    echo "skip: native-addon checks (no @parcel/watcher native binary)"
fi

echo "runtime-matrix: OK"

#!/usr/bin/env bash
# Runtime CJS/ESM contract matrix.
#
# Exercises the engine's node-services seam against a throwaway project whose
# `node_modules` is populated with npm. Run it after any `deno_runtime` tuple
# bump or change to crates/inka-runtime.
#
# usage: runtime-matrix.sh [path-to-inka]
#
# Includes jsr import-map (offline Deno cache), release-package, and
# native-addon checks. A skipped check is a failure unless
# INKA_MATRIX_ALLOW_SKIP=1 (local dev without network/deno).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
INKA="${1:-$ROOT/target/debug/inka}"

[ -x "$INKA" ] || { echo "error: inka binary not found: $INKA" >&2; exit 1; }

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/inka-matrix.XXXXXX")"
SECRET_DIR=""
SKIPS=0
cleanup() {
    rm -rf "$SCRATCH"
    [ -n "$SECRET_DIR" ] && rm -rf "$SECRET_DIR"
}
trap cleanup EXIT
skip() {
    echo "skip: $*"
    SKIPS=$((SKIPS + 1))
}

echo "== installing packages into the project node_modules =="
(cd "$SCRATCH" && npm install --no-save --omit=dev \
    ms@2.1.3 ws@8.21.3 debug@4.3.7 effect hono @parcel/watcher typescript@5.9.3 >/dev/null 2>&1)

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

echo "== pnpm-style symlinked node_modules =="
# A package reached through a symlink whose dependency lives beside its realpath
# (pnpm's isolated `.pnpm/` store), not hoisted to the project root.
mkdir -p node_modules/.pnpm/pkg-a@1.0.0/node_modules/pkg-a \
         node_modules/.pnpm/pkg-b@1.0.0/node_modules/pkg-b
printf '%s\n' '{"name":"pkg-a","version":"1.0.0","type":"module","main":"index.js"}' \
    > node_modules/.pnpm/pkg-a@1.0.0/node_modules/pkg-a/package.json
printf '%s\n' 'import b from "pkg-b";' 'export default "a+" + b;' \
    > node_modules/.pnpm/pkg-a@1.0.0/node_modules/pkg-a/index.js
printf '%s\n' '{"name":"pkg-b","version":"1.0.0","type":"module","main":"index.js"}' \
    > node_modules/.pnpm/pkg-b@1.0.0/node_modules/pkg-b/package.json
printf '%s\n' 'export default "b";' \
    > node_modules/.pnpm/pkg-b@1.0.0/node_modules/pkg-b/index.js
ln -sfn ../../pkg-b@1.0.0/node_modules/pkg-b node_modules/.pnpm/pkg-a@1.0.0/node_modules/pkg-b
ln -sfn .pnpm/pkg-a@1.0.0/node_modules/pkg-a node_modules/pkg-a
run "pnpm-style symlinked dep" "pnpm-ok a+b" p_pnpm.js \
'import a from "pkg-a";
console.log("pnpm-ok", a);'

echo "== symlink escape confinement =="
# A symlink planted inside the tree must not let a module escape the execution
# root by default. An explicit read grant (`-A`/`--allow-read`) serves the target
# (out-of-tree linked packages, Deno semantics); CJS `require()` is gated the
# same way.
ESCAPE_DIR="$SECRET_DIR/escape"
mkdir -p "$ESCAPE_DIR"
printf 'console.log("escape-loaded");\n' > "$ESCAPE_DIR/secret.js"
ln -sfn "$ESCAPE_DIR/secret.js" escape_link.js
printf '%s\n' 'import "./escape_link.js";' 'console.log("escape-import-ok");' > p_escape_import.js

# Default: the lexical symlink is canonicalized and refused.
out="$("$INKA" run p_escape_import.js 2>&1)" && {
    echo "FAIL: symlink import escape was not denied by default" >&2; printf '%s\n' "$out" >&2; exit 1
}
case "$out" in
    *"outside the execution tree"*) echo "ok: symlink import escape denied by default" ;;
    *) echo "FAIL: symlink import escape (unexpected error)" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
esac

# Explicit read grant: allowed.
out="$("$INKA" run --allow-read="$ESCAPE_DIR" p_escape_import.js 2>&1)" || {
    echo "FAIL: symlink import escape not allowed with --allow-read" >&2; printf '%s\n' "$out" >&2; exit 1
}
case "$out" in
    *"escape-import-ok"*) echo "ok: symlink import allowed with an explicit read grant" ;;
    *) echo "FAIL: symlink import with grant (unexpected output)" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
esac

mkdir -p "$ESCAPE_DIR/evilpkg"
printf '%s\n' '{"name":"evilpkg","version":"1.0.0","main":"index.js"}' > "$ESCAPE_DIR/evilpkg/package.json"
printf 'module.exports = "escape-loaded";\n' > "$ESCAPE_DIR/evilpkg/index.js"
ln -sfn "$ESCAPE_DIR/evilpkg" node_modules/evilpkg

# ESM import of an out-of-tree CJS package: deny by default...
printf '%s\n' 'import v from "evilpkg";' 'console.log("escape-pkg", v);' > p_escape_pkg.js
out="$("$INKA" run p_escape_pkg.js 2>&1)" && {
    echo "FAIL: out-of-tree package import was not denied" >&2; printf '%s\n' "$out" >&2; exit 1
}
case "$out" in
    *"outside the execution tree"*|*NotCapable*) echo "ok: out-of-tree package import denied by default" ;;
    *) echo "FAIL: out-of-tree package import (unexpected error)" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
esac

# ...and served with an explicit read grant (ESM + CJS facade).
out="$("$INKA" run --allow-read="$ESCAPE_DIR/evilpkg" p_escape_pkg.js 2>&1)" || {
    echo "FAIL: out-of-tree package import not allowed with --allow-read" >&2; printf '%s\n' "$out" >&2; exit 1
}
case "$out" in
    *"escape-pkg escape-loaded"*) echo "ok: out-of-tree package import allowed with a read grant" ;;
    *) echo "FAIL: out-of-tree package with grant (unexpected output)" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
esac

# CJS `require()` of the same out-of-tree package: deny by default...
printf '%s\n' \
    'import { createRequire } from "node:module";' \
    'const require = createRequire(import.meta.url);' \
    'try { console.log("escape-loaded", require("evilpkg")); }' \
    'catch (e) { console.log("escape-denied", e && e.constructor && e.constructor.name); }' \
    > p_escape_req.js
out="$("$INKA" run p_escape_req.js 2>&1)" || {
    echo "FAIL: require symlink escape errored unexpectedly" >&2; printf '%s\n' "$out" >&2; exit 1
}
case "$out" in
    *escape-denied*) echo "ok: require symlink escape denied by default" ;;
    *) echo "FAIL: require symlink escape was not denied" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
esac

# ...and allowed with an explicit read grant (CJS parity with ESM).
out="$("$INKA" run --allow-read="$ESCAPE_DIR/evilpkg" p_escape_req.js 2>&1)" || {
    echo "FAIL: require symlink escape with --allow-read errored" >&2; printf '%s\n' "$out" >&2; exit 1
}
case "$out" in
    *"escape-loaded escape-loaded"*) echo "ok: require symlink escape allowed with a read grant" ;;
    *) echo "FAIL: require symlink escape with grant (unexpected output)" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
esac

echo "== workspace monorepo (import map, #imports, workspace climb) =="
MONO="$SCRATCH/mono"
mkdir -p "$MONO/packages/other/src" "$MONO/packages/app/src"
printf '%s\n' '{"name":"mono","private":true,"workspaces":["packages/*"]}' > "$MONO/package.json"
printf '%s\n' '{"name":"@scope/other","type":"module","exports":{".":"./src/index.ts"}}' \
    > "$MONO/packages/other/package.json"
printf '%s\n' 'export const util = () => "mono-util";' > "$MONO/packages/other/src/util.ts"
printf '%s\n' 'import { util } from "./util.ts";' \
    'export const hello = () => `hello+${util()}`;' > "$MONO/packages/other/src/index.ts"
printf '%s\n' '{"name":"@scope/app","type":"module","bin":"src/index.ts","imports":{"#hash":"./src/hash.ts"},"dependencies":{"@scope/other":"workspace:*"}}' \
    > "$MONO/packages/app/package.json"
# Run-time aliases come from the deno.json import map. tsconfig
# baseUrl/paths are covered by their own case below.
printf '%s\n' '{"imports":{"@/lib":"./src/lib.ts"}}' > "$MONO/packages/app/deno.json"
printf '%s\n' 'export const viaHash = () => "mono-hash";' > "$MONO/packages/app/src/hash.ts"
printf '%s\n' 'export const helper = () => "mono-helper";' > "$MONO/packages/app/src/lib.ts"
printf '%s\n' 'import { hello } from "@scope/other";' \
    'import { helper } from "@/lib";' \
    'import { viaHash } from "#hash";' \
    'console.log("mono", hello(), helper(), viaHash());' > "$MONO/packages/app/src/index.ts"
mkdir -p "$MONO/packages/app/node_modules/@scope"
ln -sfn ../../../other "$MONO/packages/app/node_modules/@scope/other"

# From the workspace root (entry under cwd).
out="$(cd "$MONO" && "$INKA" run -A packages/app/src/index.ts 2>&1)" || {
    echo "FAIL: monorepo run from root" >&2; printf '%s\n' "$out" >&2; exit 1
}
case "$out" in
    *"mono hello+mono-util mono-helper mono-hash"*) echo "ok: monorepo run from workspace root" ;;
    *) echo "FAIL: monorepo run from root" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
esac

# From the member package (workspace-root climb keeps the sibling in-tree).
out="$(cd "$MONO/packages/app" && "$INKA" run -A src/index.ts 2>&1)" || {
    echo "FAIL: monorepo run from member" >&2; printf '%s\n' "$out" >&2; exit 1
}
case "$out" in
    *"mono hello+mono-util mono-helper mono-hash"*) echo "ok: monorepo run from member (workspace climb)" ;;
    *) echo "FAIL: monorepo run from member" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
esac

# Build parity (needs the launcher, which sits next to the inka binary).
LAUNCHER="${INKA_LAUNCHER:-$(dirname "$INKA")/inka-launcher}"
if [ -x "$LAUNCHER" ]; then
    (cd "$MONO" && INKA_LAUNCHER="$LAUNCHER" "$INKA" build packages/app/src/index.ts \
        -o "$MONO/app" >/dev/null 2>&1) || {
        echo "FAIL: monorepo build" >&2; exit 1
    }
    out="$("$MONO/app" 2>&1)" || {
        echo "FAIL: monorepo artifact run" >&2; printf '%s\n' "$out" >&2; exit 1
    }
    case "$out" in
        *"mono hello+mono-util mono-helper mono-hash"*) echo "ok: monorepo build parity" ;;
        *) echo "FAIL: monorepo artifact output" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
    esac
else
    skip "monorepo build parity (no inka-launcher next to $INKA)"
fi

echo "== tsconfig baseUrl/paths resolve at run time (Bun/TS parity) =="
NODENTS="$SCRATCH/nodents"
mkdir -p "$NODENTS/src"
printf '%s\n' '{ "compilerOptions": { "baseUrl": "." } }' > "$NODENTS/tsconfig.json"
printf '%s\n' 'export const u = "baseurl-ok";' > "$NODENTS/src/util.ts"
printf '%s\n' 'import { u } from "src/util";' 'console.log(u);' > "$NODENTS/main.ts"
out="$(cd "$NODENTS" && "$INKA" run -A main.ts 2>&1)" || {
    echo "FAIL: tsconfig baseUrl import" >&2; printf '%s\n' "$out" >&2; exit 1
}
case "$out" in
    *"baseurl-ok"*) echo "ok: tsconfig baseUrl resolves at run time" ;;
    *) echo "FAIL: tsconfig baseUrl output" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
esac

echo "== workspace: sibling package with an internal src/ baseUrl import =="
WS2="$SCRATCH/ws2"
mkdir -p "$WS2/packages/a/src" "$WS2/packages/b/src" "$WS2/packages/a/node_modules"
printf '%s\n' '{"name":"ws2","private":true,"workspaces":["packages/*"]}' > "$WS2/package.json"
printf '%s\n' '{"name":"a","type":"module","dependencies":{"b":"workspace:*"}}' > "$WS2/packages/a/package.json"
printf '%s\n' '{"name":"b","type":"module","exports":"./src/index.ts"}' > "$WS2/packages/b/package.json"
printf '%s\n' '{ "compilerOptions": { "baseUrl": "." } }' > "$WS2/packages/b/tsconfig.json"
printf '%s\n' 'export const name = "b-util";' > "$WS2/packages/b/src/name.ts"
printf '%s\n' 'import { name } from "src/name";' \
    'export const greet = () => `hi+${name}`;' > "$WS2/packages/b/src/index.ts"
printf '%s\n' 'import { greet } from "b";' \
    'console.log("ws2", greet());' > "$WS2/packages/a/src/index.ts"
ln -sfn ../../b "$WS2/packages/a/node_modules/b"

out="$(cd "$WS2" && "$INKA" run -A packages/a/src/index.ts 2>&1)" || {
    echo "FAIL: workspace src/ alias run from root" >&2; printf '%s\n' "$out" >&2; exit 1
}
case "$out" in
    *"ws2 hi+b-util"*) echo "ok: workspace sibling src/ alias (run from root)" ;;
    *) echo "FAIL: workspace src/ alias (run from root)" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
esac

out="$(cd "$WS2/packages/a" && "$INKA" run -A src/index.ts 2>&1)" || {
    echo "FAIL: workspace src/ alias run from member" >&2; printf '%s\n' "$out" >&2; exit 1
}
case "$out" in
    *"ws2 hi+b-util"*) echo "ok: workspace sibling src/ alias (run from member)" ;;
    *) echo "FAIL: workspace src/ alias (run from member)" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
esac

if [ -x "$LAUNCHER" ]; then
    (cd "$WS2" && INKA_LAUNCHER="$LAUNCHER" "$INKA" build packages/a/src/index.ts \
        -o "$WS2/a" >/dev/null 2>&1) || {
        echo "FAIL: workspace src/ alias build" >&2; exit 1
    }
    out="$("$WS2/a" 2>&1)" || {
        echo "FAIL: workspace src/ alias artifact run" >&2; printf '%s\n' "$out" >&2; exit 1
    }
    case "$out" in
        *"ws2 hi+b-util"*) echo "ok: workspace sibling src/ alias (build parity)" ;;
        *) echo "FAIL: workspace src/ alias artifact output" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
    esac
else
    skip "workspace src/ alias build parity (no inka-launcher next to $INKA)"
fi

echo "== workspace: --external finds a hoisted dependency =="
WSEX="$SCRATCH/wsex"
mkdir -p "$WSEX/packages/app" "$WSEX/node_modules/dep"
printf '%s\n' '{"name":"wsex","private":true,"workspaces":["packages/*"]}' > "$WSEX/package.json"
printf '%s\n' '{"name":"dep","version":"1.0.0","main":"index.js"}' > "$WSEX/node_modules/dep/package.json"
printf '%s\n' 'module.exports = { v: "dep-ok" };' > "$WSEX/node_modules/dep/index.js"
printf '%s\n' 'import dep from "dep";' \
    'console.log("ws-external", dep.v);' > "$WSEX/packages/app/index.ts"
if [ -x "$LAUNCHER" ]; then
    (cd "$WSEX/packages/app" && INKA_LAUNCHER="$LAUNCHER" "$INKA" build -A --external dep index.ts -o ./app >/dev/null 2>&1) || {
        echo "FAIL: workspace --external build" >&2; exit 1
    }
    out="$("$WSEX/packages/app/app" 2>&1)" || {
        echo "FAIL: workspace --external artifact run" >&2; printf '%s\n' "$out" >&2; exit 1
    }
    case "$out" in
        *"ws-external dep-ok"*) echo "ok: --external hoisted dep from a workspace member" ;;
        *) echo "FAIL: workspace --external output" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
    esac
else
    skip "workspace --external (no inka-launcher next to $INKA)"
fi

echo "== bundling CJS __filename/__dirname (build + run) =="
CJSFN="$SCRATCH/cjsfn"
mkdir -p "$CJSFN/node_modules/uses-filename"
printf '%s\n' '{"name":"uses-filename","version":"1.0.0","main":"index.js"}' \
    > "$CJSFN/node_modules/uses-filename/package.json"
printf '%s\n' 'exports.here = __filename;' 'exports.dir = __dirname;' \
    'exports.ok = typeof __filename === "string" && typeof __dirname === "string";' \
    > "$CJSFN/node_modules/uses-filename/index.js"
printf '%s\n' 'import * as m from "uses-filename";' \
    'console.log("cjs-filename", m.ok, m.here.includes("main.js"));' > "$CJSFN/entry.js"
if [ -x "$LAUNCHER" ]; then
    (cd "$CJSFN" && INKA_LAUNCHER="$LAUNCHER" "$INKA" build entry.js -o "$CJSFN/app" >/dev/null 2>&1) || {
        echo "FAIL: cjs __filename build" >&2; exit 1
    }
    out="$("$CJSFN/app" 2>&1)" || {
        echo "FAIL: cjs __filename artifact run" >&2; printf '%s\n' "$out" >&2; exit 1
    }
    case "$out" in
        *"cjs-filename true true"*) echo "ok: cjs __filename/__dirname shim (build parity)" ;;
        *) echo "FAIL: cjs __filename output" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
    esac
else
    skip "cjs __filename shim (no inka-launcher next to $INKA)"
fi

echo "== bundling: execution order across a re-export/barrel cycle (build + run) =="
# A valid ESM cycle (works under Bun/Node): the module defining the base class
# imports a barrel that re-exports both it and the subclass. Scope hoisting must
# initialize the base before the subclass, or `class extends` sees `undefined`.
BARCYCLE="$SCRATCH/barcycle"
mkdir -p "$BARCYCLE"
printf '%s\n' 'export { ServiceBlock } from "./service-block";' \
    'export { ActionServiceBlock } from "./action";' > "$BARCYCLE/service.ts"
printf '%s\n' 'import { ActionServiceBlock } from "./service";' \
    'export class ServiceBlock {' \
    '  make(): ServiceBlock { return new ActionServiceBlock(); }' \
    '}' > "$BARCYCLE/service-block.ts"
printf '%s\n' 'import { ServiceBlock } from "./service-block";' \
    'export class ActionServiceBlock extends ServiceBlock {}' > "$BARCYCLE/action.ts"
printf '%s\n' 'import { ServiceBlock } from "./service";' \
    'const b = new ServiceBlock();' \
    'console.log("barrel-cycle", b.make() instanceof ServiceBlock);' > "$BARCYCLE/index.ts"
if [ -x "$LAUNCHER" ]; then
    (cd "$BARCYCLE" && INKA_LAUNCHER="$LAUNCHER" "$INKA" build index.ts -o "$BARCYCLE/app" >/dev/null 2>&1) || {
        echo "FAIL: barrel-cycle build" >&2; exit 1
    }
    out="$("$BARCYCLE/app" 2>&1)" || {
        echo "FAIL: barrel-cycle artifact run" >&2; printf '%s\n' "$out" >&2; exit 1
    }
    case "$out" in
        *"barrel-cycle true"*) echo "ok: execution order (barrel cycle, build parity)" ;;
        *) echo "FAIL: barrel-cycle output" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
    esac
else
    skip "barrel-cycle execution order (no inka-launcher next to $INKA)"
fi

echo "== bundling: type-only import cycle (verbatimModuleSyntax) =="
# `tsconfig` `verbatimModuleSyntax: true` makes TypeScript keep a plain import
# that is only used in a type position. Keeping it creates a runtime cycle
# (a.ts -> b.ts -> a.ts) that breaks `class extends` at init. The bundler must
# elide type-only-used imports (as Bun/Deno do).
VBM="$SCRATCH/vbm"
mkdir -p "$VBM"
printf '%s\n' '{"compilerOptions":{"verbatimModuleSyntax":true}}' > "$VBM/tsconfig.json"
printf '%s\n' 'import { Sub } from "./b";' \
    'export class Base { make(): Sub { return null as unknown as Sub; } }' > "$VBM/a.ts"
printf '%s\n' 'import { Base } from "./a";' \
    'export class Sub extends Base {}' > "$VBM/b.ts"
printf '%s\n' 'import { Base } from "./a";' \
    'import { Sub } from "./b";' \
    'console.log("verbatim-type", new Sub() instanceof Base);' > "$VBM/index.ts"
if [ -x "$LAUNCHER" ]; then
    (cd "$VBM" && INKA_LAUNCHER="$LAUNCHER" "$INKA" build -A index.ts -o "$VBM/app" >/dev/null 2>&1) || {
        echo "FAIL: verbatim-type build" >&2; exit 1
    }
    out="$("$VBM/app" 2>&1)" || {
        echo "FAIL: verbatim-type artifact run" >&2; printf '%s\n' "$out" >&2; exit 1
    }
    case "$out" in
        *"verbatim-type true"*) echo "ok: type-only import cycle (build parity)" ;;
        *) echo "FAIL: verbatim-type output" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
    esac
else
    skip "verbatim-type cycle (no inka-launcher next to $INKA)"
fi

echo "== bundling TypeScript used at run time (package auto-embedded) =="
# An app that runs TypeScript itself (language service / createProgram) needs
# the compiler's `lib.*.d.ts` at run time. Inlining TypeScript cannot provide
# them (`ts.sys` looks for the libs next to `typescript.js`, not the bundle), so
# the bundler keeps `typescript` external and the build auto-embeds the package
# from `node_modules` — with no `--external` flag.
TSRUN="$SCRATCH/tsrun"
mkdir -p "$TSRUN"
printf '%s\n' 'import ts from "typescript";' \
    'import { existsSync } from "node:fs";' \
    'const p = ts.getDefaultLibFilePath({ target: ts.ScriptTarget.ESNext });' \
    'const sf = ts.createSourceFile("x.ts", "const a: Record<string, number> = {};", ts.ScriptTarget.ESNext, true);' \
    'console.log("tsdefaultlib", existsSync(p), sf.statements.length > 0);' > "$TSRUN/entry.ts"
if [ -x "$LAUNCHER" ]; then
    # Build from the project root so the nearest-node_modules walk finds
    # `$SCRATCH/node_modules/typescript` (as a real project would).
    (cd "$SCRATCH" && INKA_LAUNCHER="$LAUNCHER" "$INKA" build -A tsrun/entry.ts -o "$TSRUN/app" >/dev/null 2>&1) || {
        echo "FAIL: typescript auto-embed build" >&2; exit 1
    }
    out="$("$TSRUN/app" 2>&1)" || {
        echo "FAIL: typescript auto-embed artifact run" >&2; printf '%s\n' "$out" >&2; exit 1
    }
    case "$out" in
        *"tsdefaultlib true true"*) echo "ok: TypeScript package auto-embedded (build parity)" ;;
        *) echo "FAIL: typescript auto-embed output" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
    esac
    # Explicit `--external typescript` must stay equivalent.
    (cd "$SCRATCH" && INKA_LAUNCHER="$LAUNCHER" "$INKA" build -A --external typescript tsrun/entry.ts -o "$TSRUN/app-ext" >/dev/null 2>&1) || {
        echo "FAIL: typescript --external build" >&2; exit 1
    }
    out="$("$TSRUN/app-ext" 2>&1)" || {
        echo "FAIL: typescript --external artifact run" >&2; printf '%s\n' "$out" >&2; exit 1
    }
    case "$out" in
        *"tsdefaultlib true true"*) echo "ok: TypeScript --external parity (build parity)" ;;
        *) echo "FAIL: typescript --external output" >&2; printf '%s\n' "$out" >&2; exit 1 ;;
    esac
else
    skip "typescript auto-embed (no inka-launcher next to $INKA)"
fi

echo "== package entry .js -> .ts (exports rewrite) =="
# A package whose `exports` points at a `.js` entry but whose real source is
# `.ts` (the TS "write .js, ship .ts" convention). `inka run` must rewrite the
# `.js` to the `.ts` source when loading it.
mkdir -p node_modules/@scope/rewrite/src
printf '%s\n' '{"name":"@scope/rewrite","version":"1.0.0","type":"module","exports":{".":"./src/index.js"}}' \
    > node_modules/@scope/rewrite/package.json
printf '%s\n' 'export const value = () => "rewrite-ok";' > node_modules/@scope/rewrite/src/index.ts
run "package entry .js -> .ts" "rewrite rewrite-ok" p_rewrite.js \
'import { value } from "@scope/rewrite";
console.log("rewrite", value());'

echo "== npm import map subpath (URL-form npm:/pkg@ver/sub) =="
# Deno's import map expands `"debug": "npm:debug@4.3.7"` into a prefix entry
# whose value is the URL form `npm:/debug@4.3.7/`; importing a subpath then
# yields `npm:/debug@4.3.7/src/browser.js`. The runtime must parse that with
# `deno_semver` (a leading slash used to produce `invalid package name ''`).
printf '%s\n' '{"imports":{"debug":"npm:debug@4.3.7"}}' > deno.json
run "npm import-map subpath" "npm-subpath-ok function" p_npm_subpath.js \
'import createDebug from "debug/src/browser.js";
console.log("npm-subpath-ok", typeof createDebug);'
rm -f deno.json

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
        *) skip "jsr import map ($out)" ;;
    esac
else
    skip "jsr import map (no cached @std/assert in ${DENO_DIR:-$HOME/.cache/deno})"
fi

# `allow-import` is a first-class category. The custom loader confines module
# reads itself, so this guards the category's end-to-end plumbing (CLI -> DSL ->
# runtime options) against the cached jsr import.
if out="$(cd jsrproj && DENO_DIR="${DENO_DIR:-$HOME/.cache/deno}" "$INKA" run --allow-import=* main.ts 2>&1)"; then
    case "$out" in
        *jsr-ok*) echo "ok: allow-import category (cached jsr)" ;;
        *) skip "allow-import ($out)" ;;
    esac
else
    skip "allow-import (no cached @std/assert in ${DENO_DIR:-$HOME/.cache/deno})"
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
    skip "release-package checks (npm install unavailable)"
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
    skip "native-addon checks (no @parcel/watcher native binary)"
fi

if [ "$SKIPS" -gt 0 ]; then
    if [ "${INKA_MATRIX_ALLOW_SKIP:-0}" = "1" ]; then
        echo "runtime-matrix: OK ($SKIPS skipped; INKA_MATRIX_ALLOW_SKIP=1)"
        exit 0
    fi
    echo "runtime-matrix: FAILED: $SKIPS check(s) skipped (set INKA_MATRIX_ALLOW_SKIP=1 to allow)" >&2
    exit 1
fi
echo "runtime-matrix: OK"

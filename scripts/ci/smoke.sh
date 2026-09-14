#!/usr/bin/env bash
# Smoke-test a staged release before it is published.
#
# Expects a staging dir laid out like a release root:
#   install.sh, versions.json
#   inka-toolchain-<rel>-x86_64-unknown-linux-gnu.tar.gz  (+ .sha256)
#   libinka_runtime-<runtime>.so                          (+ .sha256)
#
# Installs through install.sh into a throwaway prefix/runtime-home so the host's
# own state is never touched. Exits non-zero on any failure.
#
# usage: smoke.sh <staging-dir>
set -euo pipefail

# Absolute path to this script's directory: smoke.sh `cd`s around below, so a
# relative `dirname "${BASH_SOURCE[0]}"` would not resolve afterwards.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

STAGE="$(cd "$1" && pwd)"
[ -x "$STAGE/inka" ] || { echo "error: no inka binary in $STAGE" >&2; exit 1; }
[ -f "$STAGE/install.sh" ] || { echo "error: no install.sh in $STAGE" >&2; exit 1; }

RUNTIME="$(ls "$STAGE"/libinka_runtime-*.so 2>/dev/null | head -1 | sed 's/.*libinka_runtime-\([0-9.]*\)\.so/\1/')"
[ -n "$RUNTIME" ] || { echo "error: no libinka_runtime-*.so in $STAGE" >&2; exit 1; }

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/inka-smoke.XXXXXX")"
# The --external test hides a project node_modules; restore it on any exit.
cleanup() {
    # Restore any hidden node_modules before removing the scratch dir.
    if [ -d "$SCRATCH/npm/node_modules.hidden" ] && [ ! -e "$SCRATCH/npm/node_modules" ]; then
        mv "$SCRATCH/npm/node_modules.hidden" "$SCRATCH/npm/node_modules" 2>/dev/null || true
    fi
    rm -rf "$SCRATCH"
}
trap cleanup EXIT
PREFIX="$SCRATCH/prefix"
export INKA_RUNTIME_HOME="$SCRATCH/runtime"
export HOME="$SCRATCH/home"   # keep any fallback away from the host
mkdir -p "$INKA_RUNTIME_HOME" "$HOME"

INKA="$PREFIX/lib/inka/inka"

echo "== install via install.sh (toolchain + engine) =="
sh "$STAGE/install.sh" --from "$STAGE" --yes --no-modify-path --prefix "$PREFIX"

# A pre-existing system runtime could out-rank the staged one and make
# `update` skip it; force the exact staged tuple into the scratch runtime dir.
if [ ! -f "$INKA_RUNTIME_HOME/libinka_runtime-$RUNTIME.so" ]; then
    "$INKA" update "$RUNTIME" --from "$STAGE" --home "$INKA_RUNTIME_HOME"
fi

echo "== re-run is a no-op =="
sh "$STAGE/install.sh" --from "$STAGE" --yes --no-modify-path --prefix "$PREFIX"
update_out="$("$INKA" update --from "$STAGE")"
case "$update_out" in
    *"is current"*) ;;
    *)
        echo "smoke: update did not report current:" >&2
        printf '%s\n' "$update_out" >&2
        exit 1
        ;;
esac

echo "== doctor =="
"$INKA" doctor

echo "== run + build a simple artifact =="
mkdir -p "$SCRATCH/apps" && cd "$SCRATCH/apps"
printf 'console.log("smoke-run");\n' > simple.js
out="$("$INKA" run simple.js)"
case "$out" in *smoke-run*) ;; *) echo "smoke: run output missing ($out)" >&2; exit 1 ;; esac
printf 'console.log("smoke-artifact");\n' > artifact.js
"$INKA" build artifact.js -o artifact
out="$("$SCRATCH/apps/artifact")"
case "$out" in *smoke-artifact*) ;; *) echo "smoke: artifact output missing ($out)" >&2; exit 1 ;; esac

echo "== permissions: deny-by-default + baked compile.permissions =="
mkdir -p "$SCRATCH/perms" && cd "$SCRATCH/perms"
printf 'secret\n' > secret.txt
printf 'try { Deno.readTextFileSync("secret.txt"); console.log("perm-allow"); } catch (e) { console.log("perm-denied"); }\n' > deny.js
out="$("$INKA" run deny.js)"
case "$out" in
    *perm-denied*) ;;
    *) echo "smoke: deny-by-default did not deny read ($out)" >&2; exit 1 ;;
esac
printf '{ "compile": { "permissions": { "read": ["./"] } } }\n' > deno.json
printf 'console.log("perm-allow", Deno.readTextFileSync("secret.txt").trim());\n' > allow.js
"$INKA" build allow.js -o allow
out="$("$SCRATCH/perms/allow")"
case "$out" in
    *perm-allow*) ;;
    *) echo "smoke: baked read permission did not allow ($out)" >&2; exit 1 ;;
esac

echo "== build with --external (embedded from node_modules) =="
mkdir -p "$SCRATCH/npm" && cd "$SCRATCH/npm"
npm install --no-save --omit=dev ms@2.1.3 >/dev/null 2>&1
printf 'import ms from "ms";\nconsole.log("smoke-external", ms(60000));\n' > main.js
"$INKA" build main.js -o app --external ms
mv node_modules node_modules.hidden
out="$("$SCRATCH/npm/app")"
mv node_modules.hidden node_modules
case "$out" in
    *smoke-external*) ;;
    *) echo "smoke: --external artifact failed ($out)" >&2; exit 1 ;;
esac

echo "== build with an import map -> jsr (offline at run time) =="
if command -v deno >/dev/null 2>&1; then
    mkdir -p "$SCRATCH/jsr" && cd "$SCRATCH/jsr"
    export DENO_DIR="$SCRATCH/deno"
    printf '{ "imports": { "@std/assert": "jsr:@std/assert@1" } }\n' > deno.json
    printf 'import { assertEquals } from "@std/assert";\nassertEquals(1, 1);\nconsole.log("smoke-jsr");\n' > main.ts
    if deno cache main.ts >/dev/null 2>&1; then
        "$INKA" build main.ts -o app
        # The bundle must be self-contained: run without the Deno cache.
        out="$(env -u DENO_DIR "$SCRATCH/jsr/app")"
        case "$out" in
            *smoke-jsr*) ;;
            *) echo "smoke: jsr artifact failed ($out)" >&2; exit 1 ;;
        esac
    else
        echo "skip: deno cache failed (no network?)"
    fi
else
    echo "skip: deno not installed"
fi

echo "== runtime CJS/ESM contract matrix =="
# Reuse the populated Deno cache (if any) so the matrix's jsr case can run.
if [ -d "$SCRATCH/deno" ]; then export DENO_DIR="$SCRATCH/deno"; fi
bash "$SCRIPT_DIR/runtime-matrix.sh" "$INKA"

echo "smoke: OK"

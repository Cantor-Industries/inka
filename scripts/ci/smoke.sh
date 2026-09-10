#!/usr/bin/env bash
# Smoke-test a staged release before it is published.
#
# Expects a staging dir laid out like a release root:
#   inka, inka-launcher, inka-patcher, patches/          (toolchain, adjacent)
#   libinka_runtime-<deno>.so        (+ .sha256)
#   libinka_resolver-<v>.so          (+ .sha256)
#   store.tar.gz, store.tar.gz.sha256, seed-manifest.json
#
# Runs everything in a throwaway INKA_RUNTIME_HOME / INKA_STORE so the host's
# own runtime/store are never touched. Exits non-zero on any failure.
#
# usage: smoke.sh <staging-dir>
set -euo pipefail

STAGE="$(cd "$1" && pwd)"
[ -x "$STAGE/inka" ] || { echo "error: no inka binary in $STAGE" >&2; exit 1; }

DENO="$(ls "$STAGE"/libinka_runtime-*.so 2>/dev/null | head -1 | sed 's/.*libinka_runtime-\([0-9.]*\)\.so/\1/')"
[ -n "$DENO" ] || { echo "error: no libinka_runtime-*.so in $STAGE" >&2; exit 1; }

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/inka-smoke.XXXXXX")"
trap 'rm -rf "$SCRATCH"' EXIT
export INKA_RUNTIME_HOME="$SCRATCH/runtime"
export INKA_STORE="$SCRATCH/runtime/store"
export HOME="$SCRATCH/home"   # keep the launcher's fallback away from the host
mkdir -p "$INKA_RUNTIME_HOME" "$HOME"

echo "== install runtime + resolver from staged release =="
"$STAGE/inka" update "$DENO" --from "$STAGE"

# GitHub Release assets are flat; a no-version update syncs the store from the
# same staged base (runtime/resolver are already current, so only the store moves).
echo "== seed default store from staged release =="
"$STAGE/inka" update --from "$STAGE"

echo "== doctor =="
"$STAGE/inka" doctor

echo "== store-mode imports: effect, hono, ws =="
mkdir -p "$SCRATCH/apps" && cd "$SCRATCH/apps"
for pair in \
    'eff|import { Effect } from "effect"; console.log("smoke-effect", typeof Effect.succeed);' \
    'hono|import { Hono } from "hono"; const a = new Hono(); console.log("smoke-hono", a.routes.length);' \
    'ws|import { WebSocket } from "ws"; console.log("smoke-ws", typeof WebSocket);' ; do
    name="${pair%%|*}"; code="${pair#*|}"
    printf '%s\n' "$code" > "$name.js"
    out="$("$STAGE/inka" run -A "$name.js")"
    case "$out" in
        *smoke-*) ;;
        *) echo "smoke: no expected output from $name" >&2; exit 1 ;;
    esac
done

echo "== build + run an artifact =="
printf 'console.log("smoke-artifact");\n' > artifact.js
"$STAGE/inka" build artifact.js -o artifact
out="$("$SCRATCH/apps/artifact")"
case "$out" in
    *smoke-artifact*) ;;
    *) echo "smoke: artifact output missing" >&2; exit 1 ;;
esac

echo "== permissions: deny-by-default + baked compile.permissions =="
mkdir -p "$SCRATCH/perms" && cd "$SCRATCH/perms"
printf 'secret\n' > secret.txt
printf 'try { Deno.readTextFileSync("secret.txt"); console.log("perm-allow"); } catch (e) { console.log("perm-denied"); }\n' > deny.js
out="$("$STAGE/inka" run deny.js)"
case "$out" in
    *perm-denied*) ;;
    *) echo "smoke: deny-by-default did not deny read ($out)" >&2; exit 1 ;;
esac
printf '{ "compile": { "permissions": { "read": ["./"] } } }\n' > deno.json
printf 'console.log("perm-allow", Deno.readTextFileSync("secret.txt").trim());\n' > allow.js
"$STAGE/inka" build allow.js -o allow
out="$("$SCRATCH/perms/allow")"
case "$out" in
    *perm-allow*) ;;
    *) echo "smoke: baked read permission did not allow ($out)" >&2; exit 1 ;;
esac

echo "== vendored auto-conversion (patched CJS leaf) =="
mkdir -p "$SCRATCH/vendor" && cd "$SCRATCH/vendor"
if ! "$STAGE/inka" add ms >/dev/null 2>&1; then
    echo "smoke: inka add ms failed" >&2
    exit 1
fi
[ -f "$SCRATCH/vendor/vendored/ms/esm.js" ] || { echo "smoke: ms was not auto-converted" >&2; exit 1; }

echo "smoke: OK"

#!/usr/bin/env bash
# Smoke-test a staged release before it is published.
#
# Expects a staging dir laid out like a release root:
#   install.sh, versions.json
#   inka-toolchain-<rel>-x86_64-unknown-linux-gnu.tar.gz  (+ .sha256)
#   libinka_runtime-<runtime>.so                          (+ .sha256)
#   libinka_resolver-<resolver>.so                        (+ .sha256)
#   store.tar.gz, store.tar.gz.sha256, seed-manifest.json
#
# Installs through install.sh into a throwaway prefix/store so the host's own
# runtime/store are never touched. Exits non-zero on any failure.
#
# usage: smoke.sh <staging-dir>
set -euo pipefail

STAGE="$(cd "$1" && pwd)"
[ -x "$STAGE/inka" ] || { echo "error: no inka binary in $STAGE" >&2; exit 1; }
[ -f "$STAGE/install.sh" ] || { echo "error: no install.sh in $STAGE" >&2; exit 1; }

RUNTIME="$(ls "$STAGE"/libinka_runtime-*.so 2>/dev/null | head -1 | sed 's/.*libinka_runtime-\([0-9.]*\)\.so/\1/')"
[ -n "$RUNTIME" ] || { echo "error: no libinka_runtime-*.so in $STAGE" >&2; exit 1; }

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/inka-smoke.XXXXXX")"
trap 'rm -rf "$SCRATCH"' EXIT
PREFIX="$SCRATCH/prefix"
export INKA_RUNTIME_HOME="$SCRATCH/runtime"
export INKA_STORE="$SCRATCH/runtime/store"
export HOME="$SCRATCH/home"   # keep the launcher's fallback away from the host
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
# Capture the output instead of piping into `grep -q`: grep would exit on the
# first match and close the pipe, making inka abort on EPIPE under `pipefail`.
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

echo "== store-mode imports: effect, hono, ws =="
mkdir -p "$SCRATCH/apps" && cd "$SCRATCH/apps"
for pair in \
    'eff|import { Effect } from "effect"; console.log("smoke-effect", typeof Effect.succeed);' \
    'hono|import { Hono } from "hono"; const a = new Hono(); console.log("smoke-hono", a.routes.length);' \
    'ws|import { WebSocket } from "ws"; console.log("smoke-ws", typeof WebSocket);' ; do
    name="${pair%%|*}"; code="${pair#*|}"
    printf '%s\n' "$code" > "$name.js"
    out="$("$INKA" run -A "$name.js")"
    case "$out" in
        *smoke-*) ;;
        *) echo "smoke: no expected output from $name" >&2; exit 1 ;;
    esac
done

echo "== build + run an artifact =="
printf 'console.log("smoke-artifact");\n' > artifact.js
"$INKA" build artifact.js -o artifact
out="$("$SCRATCH/apps/artifact")"
case "$out" in
    *smoke-artifact*) ;;
    *) echo "smoke: artifact output missing" >&2; exit 1 ;;
esac

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

echo "== vendored raw CommonJS + native require =="
mkdir -p "$SCRATCH/vendor" && cd "$SCRATCH/vendor"
if ! "$INKA" add ms >/dev/null 2>&1; then
    echo "smoke: inka add ms failed" >&2
    exit 1
fi
[ -f "$SCRATCH/vendor/vendored/ms/index.js" ] || { echo "smoke: ms was not vendored raw" >&2; exit 1; }
if [ -f "$SCRATCH/vendor/vendored/ms/esm.js" ]; then
    echo "smoke: unexpected esm.js (no conversion should run)" >&2
    exit 1
fi
printf '%s\n' 'import { createRequire } from "node:module";' 'const require = createRequire(import.meta.url);' 'console.log("smoke-cjs", typeof require("ms"));' > cjs.js
out="$("$INKA" run -A cjs.js)"
case "$out" in
    *smoke-cjs*) ;;
    *) echo "smoke: native require of vendored CJS failed ($out)" >&2; exit 1 ;;
esac

echo "smoke: OK"

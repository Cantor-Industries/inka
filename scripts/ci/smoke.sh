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

RUNTIME="$(ls "$STAGE"/libinka_runtime-*.so 2>/dev/null | head -1 | sed 's/.*libinka_runtime-\([0-9][0-9A-Za-z.-]*\)\.so/\1/')"
[ -n "$RUNTIME" ] || { echo "error: no libinka_runtime-*.so in $STAGE" >&2; exit 1; }

# A prerelease engine tuple is only selected in the beta channel; opt in so the
# smoke run exercises the staged engine.
case "$RUNTIME" in
    *-beta.*|*-rc.*) export INKA_CHANNEL=beta ;;
esac

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

echo "== desktop app installer =="
# Build a CEF app with `--installer`, then provision it from the staged release
# into a throwaway HOME/XDG. The CEF archive is only present in a full release
# staging dir; skip cleanly if this smoke runs against a partial one.
CEF_ARCHIVE="$(bash "$SCRIPT_DIR/laufey-asset.sh" cef x86_64-unknown-linux-gnu | awk '{print $1}')"
if [ -f "$STAGE/$CEF_ARCHIVE" ]; then
    APP_HOME="$SCRATCH/apphome"; APP_DATA="$SCRATCH/appdata"
    mkdir -p "$APP_HOME" "$APP_DATA" "$SCRATCH/deskapp" "$SCRATCH/laufey"
    # Use the staged archive as the backend source so packaging stays offline
    # and exercises the published artifact rather than re-fetching laufey.
    tar -xzf "$STAGE/$CEF_ARCHIVE" --no-same-owner --no-same-permissions -C "$SCRATCH/laufey"
    export INKA_LAUFEY_BACKEND="$SCRATCH/laufey/laufey"
    cd "$SCRATCH/deskapp"
    printf 'export default { fetch() { return new Response("smoke-desktop"); } };\n' > main.ts
    "$INKA" desktop main.ts --name SmokeApp --backend cef --installer

    APP_ID=com.inka.desktop.smokeapp
    APP_RUNTIME="$(cat "$SCRATCH/deskapp/SmokeApp/runtime-version")"
    # Drop INKA_RUNTIME_HOME so the installer proves it can provision the engine
    # into the XDG runtime dir from the release base.
    env -u INKA_RUNTIME_HOME HOME="$APP_HOME" XDG_DATA_HOME="$APP_DATA" \
        sh "$SCRATCH/deskapp/SmokeApp.install.sh" \
        --from "$SCRATCH/deskapp" --engine-base "$STAGE" --no-modify-path

    test -f "$APP_DATA/inka/runtime/libinka_runtime-$APP_RUNTIME.so" \
        || { echo "smoke: app installer did not provision the engine" >&2; exit 1; }
    test -f "$APP_DATA/inka/apps/$APP_ID/SmokeApp" \
        || { echo "smoke: app installer did not extract the app" >&2; exit 1; }
    CEF_LIB="$(ls "$APP_DATA"/cef/*/x86_64-unknown-linux-gnu/libcef.so 2>/dev/null | head -1)"
    test -n "$CEF_LIB" \
        || { echo "smoke: app installer did not provision the shared CEF runtime" >&2; exit 1; }
    test -L "$APP_HOME/.local/bin/SmokeApp" \
        || { echo "smoke: app installer did not link the launcher" >&2; exit 1; }

    if command -v xvfb-run >/dev/null 2>&1; then
        app_out="$(HOME="$APP_HOME" XDG_DATA_HOME="$APP_DATA" \
            timeout 30 xvfb-run -a "$APP_HOME/.local/bin/SmokeApp" 2>&1 || true)"
        case "$app_out" in
            *"Runtime started"*) ;;
            *) echo "smoke: desktop app did not start ($app_out)" >&2; exit 1 ;;
        esac
    else
        echo "skip: xvfb-run not installed (app launch)"
    fi
    echo "app installer: OK"
else
    echo "skip: $CEF_ARCHIVE not staged (app installer)"
fi

echo "smoke: OK"

<!--
  Maintainer notes. Everything through this closing `-->` is stripped from the
  published body by the release workflow (`.github/workflows/release.yml`), so
  it is safe to keep internal guidance here.

  The body uses `{{REL}}` (tag minus `v`, e.g. `0.7.0`) and `{{RUNTIME}}`
  (`crates/inka-runtime/runtime-version`, e.g. `0.266.6`) placeholders; the
  workflow substitutes them when staging the release. Do not hardcode versions.
-->
# inka {{REL}} — runtime tuple {{RUNTIME}}

A cleanup-and-hardening release. The engine tuple moves to `{{RUNTIME}}`, the
long-dead pre-0.6 artifact formats are removed (a breaking change — rebuild
existing artifacts), extracted trees now clean up after crashed runs, and
re-running `install.sh` on a 0.5.0+ machine upgrades in place again.

## Upgrade

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/Cantor-Industries/inka/releases/latest/download/install.sh | sh
```

Then keep the toolchain and runtime current with `inka update`.

Artifacts built by `{{REL}}` embed a `runtime>={{RUNTIME}}` floor, so run
`inka update` (or re-run `install.sh`) to install the new tuple before running
them. Older (0.6-era) artifacts keep working on the new runtime.

## Breaking changes

- **Pre-0.6 artifacts no longer run.** The legacy `INKFOOT2`/`INKFOOT3`/
  `INKFOOT4` payload formats — the single-file entry and the pre-transpiled
  TypeScript archive — are gone. `inka build` has emitted only `INKFOOT5` since
  0.5.0; rebuild any older executable with this release. The launcher refuses an
  unknown trailer instead of guessing.
- **C ABI reduced to one run entry point.** `inka_runtime_run_module_dir` is the
  only run export; `inka_runtime_run_module_perm` (single-file) was removed, and
  the `INKA_PRECOMPILED` environment flag with it. `inka_runtime_version`,
  `inka_runtime_create`/`destroy`, and the optional `inka_runtime_free_string`
  are unchanged. A runtime that lacks `_dir` still fails closed (exit 4).

## Behaviour changes

- **Runtime tuple `0.266.5` → `{{RUNTIME}}`.** New artifacts require
  `>={{RUNTIME}}` by default; existing artifacts roll forward to the new tuple.
- **`install.sh` upgrades in place again.** The previous-generation reset only
  whitelisted `0.5.x`, so re-running the installer on a 0.6.x machine deleted
  the toolchain and every runtime `.so` and re-downloaded them. Only a clearly
  pre-0.5.0 toolchain (`0.0`–`0.4`) is reset now.
- **Extracted trees reap themselves.** Each artifact tree is stamped with an
  `.inka-owner` pid marker; before staging, the launcher sweeps `inka-*` temp
  dirs whose owner pid is no longer alive (real directories this user owns;
  never symlinks). Trees with a live pid — a running artifact — are never
  touched, and legacy marker-less trees are only reaped once older than a day.
  `inka update` stamps its staging trees the same way.

## Under the hood

- Removed 12 unused dependencies (nine `rolldown_*` crates and `futures` from the
  bundler; `futures`/`serde_json` from the runtime; `url` from the CLI).
- In-crate de-duplication: one `runtime_value` helper, one manifest
  `key=value` writer, and one TS-extension predicate.
- CI now lints `inka-runtime` (`cargo clippy -D warnings`) so cdylib dead code
  cannot accumulate; the historical `attempt.md` was dropped.

## Security and hardening

These guarantees continue from 0.6, unchanged by this release:

- **Realpath module confinement.** The execution root is canonicalized once and
  every module read (ESM loader, `require()`, the graph/cache loader) must
  resolve under it — a symlink planted inside the tree cannot escape it.
- **Temp-tree hardening.** Extraction uses unpredictable, exclusive, `0700`
  directories with cleanup on drop, on an in-process `exit()` (e.g.
  `Deno.exit`), and now on a later run after an abnormal exit.
- **C ABI lifecycle.** Exported entry points are wrapped in `catch_unwind`, and
  `inka_runtime_free_string` frees runtime-allocated error strings.
- **Deny-by-default permissions.** Empty allow lists are rejected; malformed
  manifests (newline injection, bad package names, out-of-tree symlinks) are
  rejected at build time; a runtime missing `_dir` fails closed.

## Resolution

`inka run` and `inka build` resolve imports from, in order: a `deno.json` import
map, the importing file's nearest `tsconfig`/`jsconfig` `baseUrl`/`paths`, the
project/workspace `node_modules` (hoisted, nested, and symlinked pnpm/Deno
layouts), the Deno cache for `jsr:`/cached remote, then built-ins. `inka` remains
offline. `typescript` is kept external and its package is embedded automatically
when the bundle imports it at run time, so any installed version works.

Known limitations:

- `require("src/util")` (CommonJS) of a `baseUrl` path is not resolved — Deno's
  CommonJS loader searches `node_modules` paths only. Use an ESM import.
- `npm:typescript` (a Deno-cache `npm:` specifier rather than a `node_modules`
  install) is not auto-embedded; install `typescript` in the project.

## Assets

- `inka-toolchain-{{REL}}-x86_64-unknown-linux-gnu.tar.gz` — CLI + launcher
- `libinka_runtime-{{RUNTIME}}.so` — shared runtime tuple
- `install.sh` + `versions.json` — bootstrap installer + version record

`.sha256` sidecars are published for the toolchain and runtime.

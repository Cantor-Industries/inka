<!--
  Maintainer notes. Everything through this closing `-->` is stripped from the
  published body by the release workflow (`.github/workflows/release.yml`), so
  it is safe to keep internal guidance here.

  The body uses `{{REL}}` (tag minus `v`, e.g. `0.6.0`) and `{{RUNTIME}}`
  (`crates/inka-runtime/runtime-version`, e.g. `0.266.3`) placeholders; the
  workflow substitutes them when staging the release. Do not hardcode versions.
-->
# inka {{REL}} — runtime tuple {{RUNTIME}}

The runtime series: a rebuilt engine (tuple `{{RUNTIME}}`) with realpath module
confinement, temp-tree hardening, a first-class `import` permission, a sturdier
C ABI, and `tsconfig.json` `baseUrl`/`paths` resolution for monorepos.

## Upgrade

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/Cantor-Industries/inka/releases/latest/download/install.sh | sh
```

Then keep the toolchain and runtime current with `inka update`.

Artifacts built by `{{REL}}` embed a `runtime>=0.266.3` floor, so run
`inka update` (or re-run `install.sh`) to install the new tuple before running
them. Older artifacts keep working on the new runtime.

## Behaviour changes

- **Runtime tuple `0.266.2` → `{{RUNTIME}}`.** New artifacts require
  `>=0.266.3` by default; existing artifacts roll forward to the new tuple.
- **`tsconfig.json`/`jsconfig.json` `baseUrl`/`paths` now resolve** for `inka
  run` (nearest config per importing file, honoring `extends` and JSONC
  comments). A bare `src/util` or `@/*` alias no longer fails with
  `Could not find package 'src'`.
- **Running inside a workspace roots at the workspace root** (`package.json`
  `workspaces` / `deno.json` `workspace`), so sibling packages symlinked into
  `node_modules` resolve as they do in `inka build`.
- **`--external` finds workspace packages** with a nearest-`node_modules`
  lookup from the entry, not only `<cwd>/node_modules`.
- **`import` is a first-class permission category** (`--allow-import`,
  `allow-import=`). It grants Deno's import permission for cached remote/`jsr:`
  modules; the network stays disabled.
- **An empty `allow-<cat>=` list is a hard error** (write `allow-<cat>=*`).
- **`INKA_PRECOMPILED` must be exactly `1`** (presence alone no longer counts).
- **`DENO_DIR` must be absolute.** A relative cache dir is refused; the cache is
  treated as trusted input (cached JS is loaded as code).
- **Usage errors exit `2`** (from 0.5.4; restated for anyone skipping a release).

## Security and hardening

- **Realpath module confinement.** The execution root is canonicalized once and
  every module read (ESM loader, `require()`, the graph/cache loader) must
  resolve under it — a symlink planted inside the tree can no longer escape it.
- **Temp-tree hardening.** Single-file staging and artifact extraction use
  unpredictable, exclusive, `0700` directories with cleanup on drop and on an
  in-process `exit()` (e.g. `Deno.exit`).
- **C ABI lifecycle.** Exported entry points are wrapped in `catch_unwind`, and
  `inka_runtime_free_string` frees the runtime-allocated error string. The
  release profile uses `panic = "unwind"` so a panic returns an error code
  instead of aborting the host.
- **Defence-in-depth permissions.** Empty allow lists are rejected at the
  runtime DSL layer; `permissions=all` still maps to every category, including
  `import`.
- **Manifest validation** (0.5.4) continues to reject newline injection, bad
  package names, and out-of-tree dependency symlinks.

## Resolution

`inka run` and `inka build` resolve imports from, in order: a `deno.json` import
map, the importing file's nearest `tsconfig`/`jsconfig` `baseUrl`/`paths`, the
project/workspace `node_modules` (hoisted, nested, and symlinked pnpm/Deno
layouts), the Deno cache for `jsr:`/cached remote, then built-ins. `inka` remains
offline.

Known limitation: `require("src/util")` (CommonJS) of a `baseUrl` path is not
resolved — deno's CommonJS loader searches `node_modules` paths only. Use an ESM
import for `baseUrl`/`paths` aliases.

## Assets

- `inka-toolchain-{{REL}}-x86_64-unknown-linux-gnu.tar.gz` — CLI + launcher
- `libinka_runtime-{{RUNTIME}}.so` — shared runtime tuple
- `install.sh` + `versions.json` — bootstrap installer + version record

`.sha256` sidecars are published for the toolchain and runtime.

<!--
  Maintainer notes. Everything through this closing `-->` is stripped from the
  published body by the release workflow (`.github/workflows/release.yml`), so
  it is safe to keep internal guidance here.

  The body uses `{{REL}}` (tag minus `v`, e.g. `0.6.1`) and `{{RUNTIME}}`
  (`crates/inka-runtime/runtime-version`, e.g. `0.266.5`) placeholders; the
  workflow substitutes them when staging the release. Do not hardcode versions.
-->
# inka {{REL}} — runtime tuple {{RUNTIME}}

Follow-up to the 0.6 runtime series. Resolution is unified on Deno's workspace
resolver (with run-time `tsconfig` `baseUrl`/`paths` via `oxc_resolver`), the
bundler gains correctness fixes for CJS/TypeScript graphs, and TypeScript is now
carried as an embedded package instead of a version-specific lib patch.

## Upgrade

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/Cantor-Industries/inka/releases/latest/download/install.sh | sh
```

Then keep the toolchain and runtime current with `inka update`.

Artifacts built by `{{REL}}` embed a `runtime>={{RUNTIME}}` floor, so run
`inka update` (or re-run `install.sh`) to install the new tuple before running
them. Older artifacts keep working on the new runtime.

## Behaviour changes

- **Runtime tuple `0.266.3` → `{{RUNTIME}}`.** New artifacts require
  `>={{RUNTIME}}` by default; existing artifacts roll forward to the new tuple.
- **`tsconfig.json`/`jsconfig.json` `baseUrl`/`paths` resolve at run time**
  (nearest config per importing file, `extends` and JSONC comments honored, via
  the same `oxc_resolver` the bundler uses). A bare `src/util` or `@/*` alias no
  longer fails with `Could not find package 'src'` in a `src/`-layout npm
  workspace.
- **Resolution is unified on Deno's workspace resolver.** Bare specifiers go
  through the workspace import map, `#imports`, and workspace members;
  `deno.jsonc` import maps parse with comments and trailing commas; `npm:`
  version pins are enforced at build to match `inka run`.
- **`.js` specifiers rewrite to their `.ts` sibling** when the script file is
  absent (`exports: ./src/index.js` that really ships `.ts`, and relative
  `.js` → `.ts`), matching rolldown so `run` and `build` agree.
- **TypeScript is embedded as a package** when the bundle imports it at run
  time (see Bundling). It works with whatever `typescript` version is installed,
  with no flag and no per-version lib list.
- **Usage errors exit `2`** (from 0.5.4; restated for anyone skipping a release).

## Bundling

- **TypeScript is kept external and auto-embedded** whenever the emitted bundle
  imports it. The compiler resolves its `lib.*.d.ts` relative to
  `typescript.js`, so an inlined copy cannot find them; carrying the installed
  package works for every version. Type-only usage (transpiled away) is
  unaffected.
- **CJS `__filename`/`__dirname` are shimmed** to
  `import.meta.filename`/`import.meta.dirname`, so bundled CommonJS deps (e.g.
  `@effect/platform-node`) no longer throw `ReferenceError: __filename is not
  defined`.
- **Module execution order is preserved** (strict execution order plus
  on-demand wrapping), fixing `Class extends value undefined` from scope
  hoisting while keeping re-export/barrel cycles correct.
- **Type-only imports are elided** even under `verbatimModuleSyntax: true`,
  avoiding runtime cycles that break `class extends` at module init (matches
  Bun and Deno).
- **`--external` finds workspace-hoisted dependencies** and matches package
  subpaths; it remains the escape hatch for native addons and any
  asset-dependent package.
- **Bundler warnings are surfaced** (notably direct `eval`, which a
  scope-hoisted bundle cannot represent correctly).

## Security and hardening

The 0.6.0 guarantees still hold; the engine tuple changes only resolution.

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
  `import`. Manifest validation continues to reject newline injection, bad
  package names, and out-of-tree dependency symlinks.

## Resolution

`inka run` and `inka build` resolve imports from, in order: a `deno.json` import
map, the importing file's nearest `tsconfig`/`jsconfig` `baseUrl`/`paths`, the
project/workspace `node_modules` (hoisted, nested, and symlinked pnpm/Deno
layouts), the Deno cache for `jsr:`/cached remote, then built-ins. `inka` remains
offline.

Known limitations:

- `require("src/util")` (CommonJS) of a `baseUrl` path is not resolved — Deno's
  CommonJS loader searches `node_modules` paths only. Use an ESM import for
  `baseUrl`/`paths` aliases.
- `npm:typescript` (a Deno-cache `npm:` specifier rather than a `node_modules`
  install) is not auto-embedded; install `typescript` in the project so it is
  found in `node_modules`.

## Assets

- `inka-toolchain-{{REL}}-x86_64-unknown-linux-gnu.tar.gz` — CLI + launcher
- `libinka_runtime-{{RUNTIME}}.so` — shared runtime tuple
- `install.sh` + `versions.json` — bootstrap installer + version record

`.sha256` sidecars are published for the toolchain and runtime.

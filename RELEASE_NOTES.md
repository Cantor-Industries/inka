<!--
  Maintainer notes. Everything through this closing `-->` is stripped from the
  published body by the release workflow (`.github/workflows/release.yml`), so
  it is safe to keep internal guidance here.

  The body uses `{{REL}}` (tag minus `v`, e.g. `0.8.0`) and `{{RUNTIME}}`
  (`crates/inka-runtime/runtime-version`, e.g. `0.267.1`) placeholders; the
  workflow substitutes them when staging the release. Do not hardcode versions.
-->
# inka {{REL}} — runtime tuple {{RUNTIME}}

The runtime-invariants and desktop release. Resolution now hinges on a single
shared `inka-format` crate and explicit, verifiable invariants: artifacts declare
the engine capabilities they need (`requires=`), the launcher checks those
against what the installed runtime advertises, and out-of-tree packages load
only under an explicit read grant. `inka doctor` can inspect a built executable,
permission grants can be made portable across machines, and network fetching is
available opt-in while the default stays fully offline. The engine is rebased on
**Deno 2.9.7**; the runtime tuple moves to `{{RUNTIME}}`.

This release also ships **native desktop apps** (`inka desktop`) that reuse one
machine-wide engine instead of embedding a copy per app, and the first
**Windows** (`x86_64-pc-windows-msvc`) release.

## Upgrade

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/Cantor-Industries/inka/releases/latest/download/install.sh | sh
```

Then keep the toolchain and runtime current with `inka update`.

## Breaking changes

- **New runtime tuple `{{RUNTIME}}` (Deno 2.9.7).** `inka update` installs it;
  existing artifacts keep working and roll forward to the newest tuple that
  satisfies their manifest.
- **New artifacts declare `requires=` capabilities.** `inka build` records the
  engine capabilities a bundle uses (e.g. `raw-cjs` for an embedded external
  package, `native-addon` for a `.node`, `import-perm` for an `allow-import`
  grant) and raises the manifest floor accordingly. The launcher verifies them
  against the runtime's advertised set, so an artifact fails early and precisely
  on a runtime that cannot run it instead of failing at run time.
- **The default floor is now the security floor (`>=0.266.5`), not the current
  tuple.** A simple artifact therefore runs on any installed runtime from
  `0.266.5` up; a capability-dependent one requires the tuple that provides it.
- **`inka doctor <path>` treats a non-artifact as a hard error (exit 2).** The
  no-argument machine report (`inka doctor`) is unchanged.

## What's new

- **Inspect a built executable.** `inka doctor ./app` reports the artifact's
  module, runtime floor/cap, `requires=`, `path-base`, permissions, payload
  files, and whether a compatible runtime is installed. `--json` emits the same
  as machine-readable JSON; a malformed constraint or no compatible runtime
  exits 3.
- **Portable permission grants.** `${EXE_DIR}`/`${PROJECT_DIR}` tokens and
  `--path-base exe|cwd` (or `inka.path-base` in config) anchor relative
  read/write grants so an artifact's permissions travel with it. Tokens are
  expanded host-side (launcher / `inka run`); Deno's cwd-relative semantics stay
  the default. Relative CLI grants now warn when they are not anchored.
- **Capability negotiation.** The runtime advertises its capabilities via
  `inka_runtime_features()`, and `crates/inka-format` owns the security floor and
  each feature's minimum tuple; `inka build` computes the floor as the maximum of
  those. This keeps old tuples usable for simple artifacts while making a
  capability mismatch explicit.
- **Out-of-tree packages via an explicit grant.** `npm link`-style symlinked
  packages that resolve outside the execution tree are served only when an
  explicit `--allow-read`/`-A` grant covers them — for both ESM `import` and CJS
  `require()`. Deny-by-default confinement is unchanged.
- **Opt-in fetching.** `inka cache <file>` warms `$DENO_DIR/remote` for the
  remote (`jsr:`/`https:`) modules an entry needs, and `--fetch` does the same
  for a one-off `build`/`run`. The default remains fully offline.
- **CommonJS bring-your-own-`node_modules` fix.** `require()` now uses the
  standard `node_modules` lookup (including out-of-tree symlinked packages).
  inka still resolves npm only from local `node_modules`, never Deno's global npm
  cache.

## Desktop apps

`inka desktop` packages a web app as a native desktop application that **shares
the machine's inka runtime**: a prebuilt laufey window loads a small per-app
shim, which unpacks your bundle and loads the shared, desktop-enabled engine, so
an app is a few megabytes instead of embedding a ~150 MB engine. Linux (system
WebKitGTK, and `--backend cef` with a shared Chromium runtime) is supported, and
so is Windows (below); see
[Desktop apps](https://github.com/Cantor-Industries/inka/blob/master/docs/desktop.md).

- **Package** `<entry>` (an HTTP server: `export default { fetch }` or
  `Deno.serve`) or a **framework project** (`inka desktop .`: Vite, Astro, Fresh,
  Remix, React Router, SvelteKit, Nuxt, SolidStart, TanStack Start). Bundling
  matches `inka build`; `--payload <dir>` packs an already-built directory.
- **`deno.json` `desktop` config** with CLI overrides (`app.name`,
  `app.identifier`, `app.icons`, `backend`, `output`, `release.baseUrl`,
  `errorReporting.url`, `deepLinks`, `compress`).
- **Dev workflow**: `--hmr` (in-runtime Vite, external dev server, or inka's V8
  HMR) and a CDP DevTools multiplexer (`--inspect`/`--inspect-brk`/`--inspect-wait`).
- **Auto-update & error reporting**: `Deno.autoUpdate` with a signed
  `latest.json`, and uncaught JS errors + Rust panics POSTed to
  `errorReporting.url`.
- **Distribution**: a runnable app directory plus `<App>.tar.gz`/`<App>.zip`, a
  script installer on Linux (`--installer`), a Windows `.msi` (below), deep-link
  registration, and an optional self-extracting payload (`--compress`).

## Windows (`x86_64-pc-windows-msvc`) preview

Core `inka` and `inka desktop` now build and run on Windows:

- **Install** with `install.ps1` (per-user under `%LOCALAPPDATA%\inka`), then
  `inka run`/`build`/`doctor`/`update` (a built artifact's default output gains
  `.exe`; the toolchain self-update uses a rename-aside swap on the running exe).
- **`inka desktop` (webview)**: downloads the pinned laufey backend and produces
  `<App>.exe` (the renamed backend, icon embedded) + `<App>.dll` (the shim) plus
  a portable `<App>.zip`.
- **`.msi` installer**: `--installer` or `-o App.msi` builds a per-machine
  installer (pure Rust) with a Start Menu shortcut and the app icon.
- The Windows runtime is published as `libinka_runtime-{{RUNTIME}}.dll` and is
  now **desktop-enabled**; the pinned laufey backend is mirrored on the release.

## Under the hood

- **New `inka-format` crate** — one dependency-free home for artifact
  encode/parse (the embedded `inka` section, the zero-copy archive index, and the
  legacy `INKFOOT5` reader), manifest parse/render, `Version`, and
  `constraint_allows`, shared by `inka build`, the launcher, and `inka doctor`.
  Removes the build/launcher copies that could drift.
- **Artifact payload is an embedded `inka` section** (via `libsui`) instead of
  an appended `INKFOOT5` trailer, keeping the image structurally valid so it can
  be code-signed after building; older artifacts still run. See
  [Signing a built artifact](https://github.com/Cantor-Industries/inka/blob/master/docs/build.md#signing-a-built-artifact).
- **Hardened archive extraction** for toolchain/backend downloads: traversal and
  zip-symlink refusal, setuid stripping, and atomic staging.
- **Vendored from Deno** (MIT): the framework detection, the CDP DevTools
  multiplexer, hardened archive extraction, icon-set `.ico` generation, the
  Windows MSI builder, deep-link registration, and the self-extracting transform.
- **Engine rebased on Deno 2.9.7**: `deno_runtime 0.267.0`, `deno_core 0.412.0`,
  `node_resolver 0.97.0`, `deno_resolver 0.90.0`; V8 `150.4.0`, TypeScript
  `6.0.3` unchanged. The Deno pin/seam audit and the full runtime contract
  matrix were re-run against the new tuple.
- **Frozen ABI gate.** `scripts/ci/abi-symbols.sh` asserts the exported
  `inka_runtime_*` set, making an accidental ABI removal explicit.

## Assets

- `inka-toolchain-{{REL}}-x86_64-unknown-linux-gnu.tar.gz` — CLI + launcher +
  desktop shim (`libinka_desktop_shim.so`)
- `libinka_runtime-{{RUNTIME}}.so` — shared runtime tuple (desktop-enabled)
- `inka-toolchain-{{REL}}-x86_64-pc-windows-msvc.zip` — Windows CLI + launcher +
  desktop shim (`libinka_desktop_shim.dll`)
- `libinka_runtime-{{RUNTIME}}.dll` — Windows shared runtime (desktop-enabled)
- `laufey-cef-*.tar.gz` (Linux) and `laufey-webview-*.zip` (Windows) — pinned
  backend mirrors
- `install.sh` / `install.ps1` + `versions.json` — bootstrap installers + version
  record

`.sha256` sidecars are published for the toolchains and runtimes.

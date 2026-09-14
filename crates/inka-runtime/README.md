# inka-runtime

The real engine: a `cdylib` built on Deno (`deno_runtime`/`deno_core`) that the
launcher (and `inka run`) dlopen. One installed runtime file = one version
tuple: `libinka_runtime-<v>.so`, where `<v>` is the `deno_runtime` base
(`0.xxx.0`, e.g. `0.266.0` ↔ Deno 2.9.x) plus an inka runtime revision (`.1`,
`.2`, …), tracked in `runtime-version`. Artifacts never embed it — they load the
best matching installed tuple.

## C ABI

- `inka_runtime_version()` — version string reported by the runtime,
- `inka_runtime_create()` / `inka_runtime_destroy()` — session lifecycle,
- `inka_runtime_run_module_perm()` — permission-aware single-file entry,
- `inka_runtime_run_module_dir()` — multi-file: a `dir` root + `entry` path,
  resolving relative imports, with runtime TS transpile per file,
- `inka_runtime_free_string()` — free an `err_msg` string the runtime returned.

The run entry points are wrapped in `catch_unwind`, so a panic surfaces as an
error code + message rather than aborting the host.

There is no permission-less entry point. A runtime missing `_perm`/`_dir`
causes the launcher to fail closed (exit 4) rather than run with wrong
permissions.

## Permissions

Deny-by-default. The manifest/`inka run` DSL (`permissions=all|none`,
`allow-<cat>`, `deny-<cat>`) maps onto the Deno permission model; prompts are
disabled. Categories are `read`, `write`, `net`, `env`, `run`, `sys`, `ffi`,
`import` (`import` covers cached remote/`jsr:` modules; the network stays
disabled). Reads `INKA_PRECOMPILED` (must be exactly `1` to serve an archive's
already-transpiled TS as JS). Resolution is rooted at the execution tree, and
every `file:` load is confined to its realpath (symlinks cannot escape).
`DENO_DIR` is read for `jsr:` and other cached remote modules; it must be an
absolute path, and the cache is **trusted input** (cached JS is loaded as code).

## Resolution

`resolver.rs` builds an offline `deno_graph::ModuleGraph` for the entry from
`$DENO_DIR` (a `GlobalHttpCache` loader + an `import_map` resolver), then exposes
synchronous lookups. `tsconfig.rs` resolves bare `baseUrl`/`paths` aliases
(e.g. `src/util` under `"baseUrl": "."`, or `@/*`) from the importing file's
nearest `tsconfig.json`/`jsconfig.json`, honoring `extends` and JSONC, and
confines results to the execution tree. `node_services.rs` is the CJS/`require()`
seam over Deno's `deno_node`/`node_resolver` machinery:

- `require()` runs natively (CJS→CJS, Node builtins, nested deps, cycles),
- ESM `import` of a CJS package is served as an ESM facade (default plus
  statically-detected named exports) via `node_resolver::analyze`,
- `require()` of an ESM package returns the namespace.

Classification: app code defaults to ESM; `.js` under a package root defaults to
CJS; `.cjs`/`.cts` are CJS; `.mjs`/`.mts`/`.json` are not. The analyzer parses
the source, so an ESM file inside a package root passes through unchanged.

## Tuple updates

The Deno crates are not a stable API, so the runtime pins them exactly
(`deno_runtime = "=0.266.0"` and friends) and touches them only through the
`node_services.rs` / `resolver.rs` seams and `build.rs`. To bump a tuple, see
[`docs/runtime-tuple-update.md`](../../docs/runtime-tuple-update.md).

## Build (heavy)

This pulls the full Deno/V8 tree and a one-time snapshot build; point cargo at a
roomy disk:

```sh
CARGO_HOME=… CARGO_TARGET_DIR=… cargo build --release -p inka-runtime
# install as a tuple (version comes from runtime-version):
cp $CARGO_TARGET_DIR/release/libinka_runtime.so \
   ~/.local/share/inka/runtime/libinka_runtime-$(cat runtime-version).so
```

## License

MIT — see `LICENSE` in this directory (Copyright (c) 2026 Cantor Industries
Authors).

## Acknowledgments — Deno

This crate embeds the Deno runtime: `deno_core`, `deno_runtime`, `deno_error`,
`deno_semver`, `deno_graph`, `deno_cache_dir`, `import_map` (and `deno_ast` for
build-time snapshot generation). Deno is the work of the Deno authors,
Copyright (c) the Deno authors, distributed under the MIT license (portions
Apache-2.0); see <https://github.com/denoland/deno>.

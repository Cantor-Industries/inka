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
  resolving relative imports, with runtime TS transpile per file.

There is no permission-less entry point. A runtime missing `_perm`/`_dir`
causes the launcher to fail closed (exit 4) rather than run with wrong
permissions.

## Permissions

Deny-by-default. The manifest/`inka run` DSL (`permissions=all|none`,
`allow-<cat>`, `deny-<cat>`) maps onto the Deno permission model; prompts are
disabled. Reads a few env vars set by the launcher/CLI: `INKA_STORE`,
`INKA_VENDOR`, and `INKA_PRECOMPILED` (serve an archive's already-transpiled TS
as JS).

## CommonJS / node services

`node_services.rs` is the **single seam** over Deno's `deno_node`/`node_resolver`
machinery. It backs Deno's native CJS loader with the inka store:

- `require()` runs natively (CJS→CJS, Node builtins, nested deps, cycles),
- ESM `import` of a CJS package is served as an ESM facade (default plus
  statically-detected named exports) via `node_resolver::analyze`,
- `require()` of an ESM package returns the namespace.

Classification: app code defaults to ESM; `.js` in the store/vendored roots
defaults to CJS; `.cjs`/`.cts` are CJS; `.mjs`/`.mts`/`.json` are not. The
analyzer parses the source, so an ESM file inside a package root passes through
unchanged.

## Tuple updates

The Deno crates are not a stable API, so the runtime pins them exactly
(`deno_runtime = "=0.266.0"` and friends) and touches them only through the
`node_services.rs` seam and `build.rs`. To bump a tuple, see
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

See the repository README "Building the real runtime".

## License

MIT — see `LICENSE` in this directory (Copyright (c) 2026 Cantor Industries
Authors).

## Acknowledgments — Deno

This crate embeds the Deno runtime: `deno_core`, `deno_runtime`, `deno_error`,
`deno_semver` (and `deno_ast` for build-time snapshot generation). Deno is the
work of the Deno authors, Copyright (c) the Deno authors, distributed under the
MIT license (portions Apache-2.0); see <https://github.com/denoland/deno>.

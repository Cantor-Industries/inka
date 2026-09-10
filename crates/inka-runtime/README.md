# inka-runtime

The real engine: a `cdylib` built on Deno (`deno_runtime`/`deno_core`) that the
launcher (and `inka run`) dlopen. One installed runtime file = one version
tuple: `libinka_runtime-<v>.so`, where `<v>` is the pinned `deno_runtime`
version (e.g. `0.266.0` ↔ Deno 2.9.x). Artifacts never embed it — they load the
best matching installed tuple.

## C ABI

- `inka_runtime_version()` — version string reported by the runtime,
- `inka_runtime_create()` / `inka_runtime_destroy()` — session lifecycle,
- `inka_runtime_run_module()` — legacy single-file entry (deny-by-default on
  current builds),
- `inka_runtime_run_module_perm()` — additive, permission-aware single-file,
- `inka_runtime_run_module_dir()` — multi-file: a `dir` root + `entry` path,
  resolving relative imports, with runtime TS transpile per file.

Missing `_perm`/`_dir` symbols on an old runtime cause the launcher to fail
closed (exit 4) rather than run with wrong permissions.

## Permissions

Deny-by-default. The manifest/`inka run` DSL (`permissions=all|none`,
`allow-<cat>`, `deny-<cat>`) maps onto the Deno permission model; prompts are
disabled. Reads a few env vars set by the launcher/CLI: `INKA_STORE`,
`INKA_VENDOR`, `INKA_RESOLVER`, and `INKA_PRECOMPILED` (serve an archive's
already-transpiled TS as JS).

## Build (heavy)

This pulls the full Deno/V8 tree and a one-time snapshot build; point cargo at a
roomy disk:

```sh
CARGO_HOME=… CARGO_TARGET_DIR=… cargo build --release -p inka-runtime
# install as a tuple:
cp $CARGO_TARGET_DIR/release/libinka_runtime.so ~/.local/share/inka/runtime/libinka_runtime-0.266.0.so
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

# inka-launcher

The tiny host that every inka artifact is built on. `inka build` appends your
source + manifest onto this launcher; when you run the resulting file, the
launcher:

1. parses its own trailer (single-file payload, or an `INKFOOT3`/`INKFOOT4`
   multi-file archive, optionally TS pre-transpiled),
2. finds a compatible runtime tuple (`libinka_runtime-<v>.so`) under
   `INKA_RUNTIME_HOME` / `~/.local/share/inka/runtime` / `/usr/local/lib/inka-runtime`
   matching the manifest's `runtime=` floor and `tested-against=` cap,
3. dlopens it and calls the frozen C ABI
   (`inka_runtime_create` / `inka_runtime_destroy` /
   `inka_runtime_run_module_perm` / `inka_runtime_run_module_dir`) with the
   payload, argv, and the manifest's permission DSL,
4. fails closed (exit 4) if the installed runtime lacks the `_perm`/`_dir`
   entry points. There is no permission-less fallback, so deny-by-default can
   never degrade to allow-all.

It also sets up the environment the runtime reads: default `INKA_STORE` to a
`store/` dir next to the runtime, default `INKA_RESOLVER` to the newest
installed resolver, and `INKA_VENDOR` to the artifact's embedded `vendored/`
tree (cleared otherwise, so a caller-exported `INKA_VENDOR` never leaks in).

## Build

```sh
cargo build -p inka-launcher     # target/debug/inka-launcher
```

Keep it next to the `inka` binary (or set `INKA_LAUNCHER`).

## License

MIT — see `LICENSE` in this directory (Copyright (c) 2026 Cantor Industries
Authors).

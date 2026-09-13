# inka-launcher

The tiny host that every inka artifact is built on. `inka build` appends the
bundle (+ any embedded files) and manifest onto this launcher; when you run the
resulting file, the launcher:

1. parses its own trailer (an `INKFOOT5` bundle + embedded files, or a legacy
   `INKFOOT2`/`INKFOOT3`/`INKFOOT4` payload),
2. finds a compatible runtime tuple (`libinka_runtime-<v>.so`) under
   `INKA_RUNTIME_HOME` / `~/.local/share/inka/runtime` matching the manifest's
   `runtime=` floor and `tested-against=` cap,
3. dlopens it and calls the frozen C ABI
   (`inka_runtime_create` / `inka_runtime_destroy` /
   `inka_runtime_run_module_perm` / `inka_runtime_run_module_dir`) with the
   payload, argv, and the manifest's permission DSL,
4. fails closed (exit 4) if the installed runtime lacks the `_perm`/`_dir`
   entry points. There is no permission-less fallback, so deny-by-default can
   never degrade to allow-all.

Archive artifacts are extracted to a temp tree and run with that tree as the
execution root, so embedded `node_modules/…` files (from `--external`) resolve
there.

## Build

```sh
cargo build -p inka-launcher     # target/debug/inka-launcher
```

Keep it next to the `inka` binary (or set `INKA_LAUNCHER`).

## License

MIT — see `LICENSE` in this directory (Copyright (c) 2026 Cantor Industries
Authors).

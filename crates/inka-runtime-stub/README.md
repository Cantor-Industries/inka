# inka-runtime-stub

A tiny stub runtime used for launcher development and spikes. It implements the
same C ABI as the real `inka-runtime` (`inka_runtime_run_module_perm` +
`inka_runtime_run_module_dir`) but does no work — it echoes what the launcher
asked for and returns a configured exit code, which is enough to test tuple
discovery, roll-forward vs `tested-against=` pinning, and archive extraction
without building the heavyweight Deno engine.

Builds in seconds (`scripts/spike.sh` uses it to stand up `libinka_runtime-0.0.0.so`
and `-0.1.0.so` tuples).

## Environment

- `INKA_STUB_VERSION` — version reported by `inka_runtime_version()` and used
  as the tuple suffix,
- `INKA_STUB_ECHO=1` — echo the module/spec/args the launcher passed (see
  `scripts/spike.sh`).

## Build

```sh
cargo build --release -p inka-runtime-stub
cp target/release/libinka_runtime_stub.so ~/.local/share/inka/runtime/libinka_runtime-<ver>.so
```

## License

MIT — see `LICENSE` in this directory (Copyright (c) 2026 Cantor Industries
Authors).

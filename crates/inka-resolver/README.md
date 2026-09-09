# inka-resolver

Pure-Rust import-resolution engine, built as both a `cdylib` (dlopened by the
runtime) and an `rlib` (linked by the inka CLI for `--vendor-closure` builds).
It has **no** deno/V8 dependency — only `deno_semver`, `serde_json`, and `url` —
so it rebuilds in seconds.

## Resolution model

`resolve_v2(store, vendor, referrer, specifier)` returns one of:

- `UseDefault` — let the engine's own resolver handle relative/`/`/`node:`/`file:`
  specifiers,
- `File(path)` — serve a concrete file (store/vendored package resolved through
  its `exports` map),
- `Builtin("node:…")` — a Node built-in,
- `Error(msg)` — a clean, actionable failure.

Two tiers:

- **Store-internal** imports keep store-pool + builtins semantics and never see
  a project's `vendored/`.
- **App/vendored** code resolves bare and pinned specifiers as
  vendored → default store → builtins.

Only ESM-capable `exports` targets are selected (`import`/`node`/`default`; a
CommonJS-only target is rejected — the engine is ESM-only). A `vendored/` entry
counts as a package only when its `package.json` exists, so stray dirs are
skipped cleanly.

## C ABI (cdylib)

`inka_resolver_resolve(...)` returns a `KIND_*` code with an owned result
string; `inka_resolver_abi()` reports the ABI number (2) the runtime checks.

## Build & test

```sh
cargo build -p inka-resolver
cargo test -p inka-resolver
```

## License

MIT — see `LICENSE` in this directory (Copyright (c) 2026 Cantor Industries
Authors).

## Acknowledgments — Deno

This crate uses `deno_semver` (a Deno-project crate) for version parsing and
semantics. Deno is Copyright (c) the Deno authors, distributed under the MIT
license (portions Apache-2.0); see <https://github.com/denoland/deno>.

# inka (CLI)

The inka command-line toolchain: packs your source into a single-file
executable (launcher + payload + manifest) that loads a shared per-machine
Deno runtime tuple, and manages that runtime, the package store, and per-project
vendoring.

See the [repository README](../../README.md) for the full architecture and
`docs/deployment.md` for distribution.

## Commands

- `inka build` — pack a `.ts`/`.js` entry (and its import graph) onto the
  `inka-launcher` into an executable. Permission lines are baked only from
  explicit build-intent sources (`-P <set>`, `deno.json compile.permissions`, an
  `inka.permissions` marker); otherwise the artifact is deny-by-default.
  `--vendor-closure` embeds only reachable vendored modules; `--no-vendor`
  relies on the machine default store.
- `inka run` — execute a `.ts`/`.js` file directly through the installed
  runtime (deno-run-style permission flags, `-A`/`-P`/granular `--allow-*`).
- `inka install <ver> --from <base>` — install a runtime tuple + resolver +
  optional store payload into `~/.inka-runtime` (sha256-verified).
- `inka list` / `inka doctor` — show installed tuples / a diagnostic report.
- `inka add` / `inka remove` — per-project vendoring into `vendored/`,
  including automatic and curated CJS→ESM conversion.
- `inka vendor …` — vendored-set list/status and git posture.
- `inka pkg snapshot|seed|list` — build/install/inspect the default store.

## Build & test

```sh
cargo build -p inka            # debug (target/debug/inka)
cargo test -p inka --bin inka  # unit tests (config, embed, vendor, run)
```

The launcher must be next to the binary (`target/debug/inka-launcher`) or found
via `INKA_LAUNCHER`.

## License

MIT — see `LICENSE` in this directory (Copyright (c) 2026 Cantor Industries
Authors).

## Acknowledgments — Deno

This crate uses `deno_ast` (a Deno-project crate) for optional build-time
TypeScript→JavaScript transpilation (`--transpile`). Deno is Copyright (c) the
Deno authors, distributed under the MIT license (portions Apache-2.0); see
<https://github.com/denoland/deno>.

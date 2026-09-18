# inka (CLI)

The inka command-line toolchain: bundles a JS/TS project into a single-file
executable (launcher + bundle + manifest) that loads a shared per-user Deno
runtime tuple, and manages that runtime.

See the [repository README](../../README.md) for the full architecture and
`docs/deployment.md` for distribution.

## Commands

- `inka build` — bundle a `.ts`/`.js` entry (import maps + `npm:`/`jsr:` +
  `node_modules`) into one self-contained module and pack it onto
  `inka-launcher`. `--minify`, `--sourcemap`, `--external <pkg>`, and
  `--embed-dir` control the bundle. Permission lines bake from explicit
  build-intent sources, in precedence order: CLI grant flags
  (`-A`/`--allow-*`, which override config), `deno.json compile.permissions`,
  or an `inka.permissions` marker (a set name, `"all"`, or an inline category
  map — usable in `package.json`). With no source the artifact is
  deny-by-default and `build` warns.
- `inka run` — execute a `.ts`/`.js` file directly through the installed
  runtime (deno-run-style permission flags, `-A`/`-P`/granular `--allow-*`),
  resolving import maps, `npm:` (node_modules), and `jsr:` (Deno cache, offline).
- `inka update [<ver>] [--from <base>]` — self-update the toolchain (for
  installer-managed installs) and reconcile the shared runtime with the newest
  release (or install a specific tuple), sha256-verified. Downloads show a
  progress bar on a TTY.
- `inka doctor` — a grouped diagnostic report (runtimes + project status with
  status glyphs and per-problem hints). Given an executable
  (`inka doctor <artifact>`), it inspects the artifact instead (module, runtime
  floor, permissions, payload, and runtime compatibility); `--json` for
  machine-readable output.
- `inka help [command]` — short (`-h`) or full (`--help`) help; running `inka`
  with no arguments prints the top-level help.

Output is colored on a TTY (honors `NO_COLOR`/`FORCE_COLOR`); `INK_LOG`,
`INK_LOG_STYLE`, `-q`/`--quiet`, and `-v`/`--verbose` adjust verbosity.

Bundling lives behind the non-default `bundle` cargo feature (release builds
enable it); without it, `inka build` reports that bundling is unavailable.

## Build & test

```sh
cargo build -p inka --features bundle   # debug, with bundling
cargo build -p inka                     # lean (no rolldown)
cargo test -p inka --bin inka           # unit tests (config, embed, run, build)
```

The launcher must be next to the binary (`target/debug/inka-launcher`) or found
via `INKA_LAUNCHER`.

## License

MIT — see `LICENSE` in this directory (Copyright (c) 2026 Cantor Industries
Authors).

## Acknowledgments — Deno

This crate links the `inka-bundler` crate, which uses Deno-project crates
(`deno_graph`, `deno_cache_dir`, `import_map`) for offline resolution, and
[rolldown](https://rolldown.rs) for bundling. Deno is Copyright (c) the Deno
authors, distributed under the MIT license (portions Apache-2.0); see
<https://github.com/denoland/deno>.

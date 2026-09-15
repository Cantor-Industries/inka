# inka-bundler

Resolves a JS/TS entry offline and bundles it into one self-contained ESM module
with [rolldown](https://rolldown.rs). Used by `inka build`.

## What it does

`bundle(BundleOptions { cwd, entry, external, minify, sourcemap })`:

1. builds a `deno_graph::ModuleGraph` for the entry from the local Deno cache
   (`$DENO_DIR`) — a `deno.json` import map maps bare specifiers, `jsr:` (and
   remote `https:`) modules are read from `$DENO_DIR/remote`, all offline;
2. runs rolldown with a `DenoResolvePlugin` that applies the import map, routes
   `npm:` to `node_modules`, resolves `jsr:` to its cached `https://jsr.io/…`
   source, and serves those sources (`ModuleType::Ts`);
3. returns `{ code, embedded, warnings, auto_embed }`, where `embedded` are
   native `.node` candidates for the caller to pack and `auto_embed` names the
   default-external packages the emitted chunk imports.

`external` packages are left unbundled (the caller embeds them from
`node_modules`); `node:` built-ins stay external (the engine provides them).

`typescript` is kept external by default (`DEFAULT_EXTERNAL`) and reported via
`auto_embed` when the chunk imports it, so the caller embeds the installed
package — an inlined TypeScript cannot resolve its `lib.*.d.ts` at run time.

## Notes

- Resolution and bundling are **offline**: nothing is fetched from the network.
- `deno_graph` is not a transitive dependency of `deno_runtime`, so it is pinned
  directly (along with `deno_cache_dir` and `import_map`).
- The rolldown family is pinned exactly (`=1.2.7`); an unpinned patch release
  pulled a newer `oxc` and broke `rolldown_common`.

## Build & test

```sh
cargo test -p inka-bundler
```

## License

MIT — see `LICENSE` in this directory (Copyright (c) 2026 Cantor Industries
Authors).

## Acknowledgments — Deno

Uses Deno-project crates (`deno_graph`, `deno_cache_dir`, `import_map`) and
rolldown. Deno is Copyright (c) the Deno authors, distributed under the MIT
license (portions Apache-2.0); see <https://github.com/denoland/deno>.

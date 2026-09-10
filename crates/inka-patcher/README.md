# inka-patcher

Standalone workspace that converts CommonJS packages into engine-viable pure
ESM for the ESM-only inka engine. It bundles a CJS package's entry with
rolldown, post-processes the output (hoist `node:` builtin requires, replace the
`createRequire` shim with a catchable throwing require, neutralize selected
`process.env` reads), writes an `esm.js` bundle, and rewrites the package's
`exports` so the `import` condition serves it.

Because it depends on the heavy rolldown/oxc stack (and pins `tokio` differently
than `inka-runtime`), it is **its own nested workspace** with an independent
`Cargo.lock` — not a root-workspace member.

## Patch specs

Curated specs live under `patches/<pkg>/<version>/patch.json` (repo root):

```json
{
  "package": "ws",
  "version": "8.21.3",
  "type": "bundle-esm",                 // or "file-patch"
  "entry": "wrapper.mjs",
  "external": ["bufferutil", "utf-8-validate"],
  "output": "esm.js",
  "neutralizeEnv": ["WS_NO_BUFFER_UTIL"]
}
```

- `bundle-esm` — rolldown-bundle `entry` to `output` with `external`/`neutralizeEnv`.
- `file-patch` — truncate `file` at `deleteFromMarker`.

## Invocation

`inka-patcher apply --spec <patch.json> --node-modules <dir>` applies the patch
in place to an installed package. Callers:

- `inka add` — applies a curated spec when present, or auto-converts a CJS leaf
  by synthesizing a `bundle-esm` spec (hard cases name the exact
  `patches/<pkg>/<version>/patch.json` to create).
- `inka internal snapshot-store` — patches curated CJS leaves (ws/undici/mime/msgpackr)
  into the store snapshot.

## Build (heavy)

Build from inside this directory with the big-disk cargo home/target:

```sh
cd crates/inka-patcher
CARGO_HOME=… CARGO_TARGET_DIR=… cargo build --release
```

Keep the resulting `inka-patcher` next to the inka binary or set `INKA_PATCHER`.

## License

MIT — see `LICENSE` in this directory (Copyright (c) 2026 Cantor Industries
Authors).

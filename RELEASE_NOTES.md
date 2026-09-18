<!--
  Maintainer notes. Everything through this closing `-->` is stripped from the
  published body by the release workflow (`.github/workflows/release.yml`), so
  it is safe to keep internal guidance here.

  The body uses `{{REL}}` (tag minus `v`, e.g. `0.8.0`) and `{{RUNTIME}}`
  (`crates/inka-runtime/runtime-version`, e.g. `0.267.1`) placeholders; the
  workflow substitutes them when staging the release. Do not hardcode versions.
-->
# inka {{REL}} — runtime tuple {{RUNTIME}}

The runtime-invariants release. Resolution now hinges on a single shared
`inka-format` crate and explicit, verifiable invariants: artifacts declare the
engine capabilities they need (`requires=`), the launcher checks those against
what the installed runtime advertises, and out-of-tree packages load only under
an explicit read grant. `inka doctor` can inspect a built executable, permission
grants can be made portable across machines, and network fetching is available
opt-in while the default stays fully offline. The engine is rebased on **Deno
2.9.7**; the runtime tuple moves to `{{RUNTIME}}`.

## Upgrade

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/Cantor-Industries/inka/releases/latest/download/install.sh | sh
```

Then keep the toolchain and runtime current with `inka update`.

## Breaking changes

- **New runtime tuple `{{RUNTIME}}` (Deno 2.9.7).** `inka update` installs it;
  existing artifacts keep working and roll forward to the newest tuple that
  satisfies their manifest.
- **New artifacts declare `requires=` capabilities.** `inka build` records the
  engine capabilities a bundle uses (e.g. `raw-cjs` for an embedded external
  package, `native-addon` for a `.node`, `import-perm` for an `allow-import`
  grant) and raises the manifest floor accordingly. The launcher verifies them
  against the runtime's advertised set, so an artifact fails early and precisely
  on a runtime that cannot run it instead of failing at run time.
- **The default floor is now the security floor (`>=0.266.5`), not the current
  tuple.** A simple artifact therefore runs on any installed runtime from
  `0.266.5` up; a capability-dependent one requires the tuple that provides it.
- **`inka doctor <path>` treats a non-artifact as a hard error (exit 2).** The
  no-argument machine report (`inka doctor`) is unchanged.

## What's new

- **Inspect a built executable.** `inka doctor ./app` reports the artifact's
  module, runtime floor/cap, `requires=`, `path-base`, permissions, payload
  files, and whether a compatible runtime is installed. `--json` emits the same
  as machine-readable JSON; a malformed constraint or no compatible runtime
  exits 3.
- **Portable permission grants.** `${EXE_DIR}`/`${PROJECT_DIR}` tokens and
  `--path-base exe|cwd` (or `inka.path-base` in config) anchor relative
  read/write grants so an artifact's permissions travel with it. Tokens are
  expanded host-side (launcher / `inka run`); Deno's cwd-relative semantics stay
  the default. Relative CLI grants now warn when they are not anchored.
- **Capability negotiation.** The runtime advertises its capabilities via
  `inka_runtime_features()`, and `crates/inka-format` owns the security floor and
  each feature's minimum tuple; `inka build` computes the floor as the maximum of
  those. This keeps old tuples usable for simple artifacts while making a
  capability mismatch explicit.
- **Out-of-tree packages via an explicit grant.** `npm link`-style symlinked
  packages that resolve outside the execution tree are served only when an
  explicit `--allow-read`/`-A` grant covers them — for both ESM `import` and CJS
  `require()`. Deny-by-default confinement is unchanged.
- **Opt-in fetching.** `inka cache <file>` warms `$DENO_DIR/remote` for the
  remote (`jsr:`/`https:`) modules an entry needs, and `--fetch` does the same
  for a one-off `build`/`run`. The default remains fully offline.
- **CommonJS bring-your-own-`node_modules` fix.** `require()` now uses the
  standard `node_modules` lookup (including out-of-tree symlinked packages).
  inka still resolves npm only from local `node_modules`, never Deno's global npm
  cache.

## Under the hood

- **New `inka-format` crate** — one dependency-free home for the `INKFOOT5`
  footer/archive encode + parse, the zero-copy archive index, manifest
  parse/render, `Version`, and `constraint_allows`, shared by `inka build`, the
  launcher, and `inka doctor`. Removes the build/launcher copies that could
  drift.
- **Engine rebased on Deno 2.9.7**: `deno_runtime 0.267.0`, `deno_core 0.412.0`,
  `node_resolver 0.97.0`, `deno_resolver 0.90.0`; V8 `150.4.0`, TypeScript
  `6.0.3` unchanged. The Deno pin/seam audit and the full runtime contract
  matrix were re-run against the new tuple.
- **Frozen ABI gate.** `scripts/ci/abi-symbols.sh` asserts the exported
  `inka_runtime_*` set, making an accidental ABI removal explicit.

## Assets

- `inka-toolchain-{{REL}}-x86_64-unknown-linux-gnu.tar.gz` — CLI + launcher
- `libinka_runtime-{{RUNTIME}}.so` — shared runtime tuple
- `install.sh` + `versions.json` — bootstrap installer + version record

`.sha256` sidecars are published for the toolchain and runtime.

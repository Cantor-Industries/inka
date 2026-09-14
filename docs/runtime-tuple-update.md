# Runtime tuple updates

The inka engine is a `cdylib` built against a specific Deno release. The Deno
crates (`deno_runtime`, `deno_core`, `deno_node`, `node_resolver`,
`deno_graph`, `deno_cache_dir`, …) are **not a stable public API**: every
cross-crate type must match the Deno tag's own `Cargo.lock`. A tuple update is
therefore a deliberate, tested change, not a routine dependency bump.

A tuple is `libinka_runtime-<v>.so`, where `<v>` is the pinned `deno_runtime`
base (`0.xxx.0`) plus an inka revision (`.1`, `.2`, …), tracked in
`crates/inka-runtime/runtime-version`. Behavior-only changes (no Deno bump)
advance the revision; a Deno bump moves the base and restarts the revision.

## The seam

All Deno API surface is confined so a bump stays contained:

- `crates/inka-runtime/src/node_services.rs` — the CJS/`require()` seam over
  `deno_node` / `node_resolver` / `deno_core` types;
- `crates/inka-runtime/src/resolver.rs` — the offline module graph over
  `deno_graph` / `deno_cache_dir` / `import_map`;
- `crates/inka-runtime/build.rs` — the V8 snapshot + residual lazy sources.

`crates/inka-bundler` also pins Deno crates (`deno_graph`, `deno_cache_dir`,
`import_map`) for build-time resolution.

## Bump procedure

1. **Move the pins together.** Update the exact pins in
   `crates/inka-runtime/Cargo.toml` and `crates/inka-bundler/Cargo.toml`
   (`deno_core`, `deno_runtime`, `deno_node` if named, `node_resolver`,
   `deno_semver`, `deno_error`, `sys_traits`, `deno_ast`, `deno_graph`,
   `deno_cache_dir`, `deno_lockfile`, `deno_npm`, `import_map`) to the versions
   from the target Deno tag's `Cargo.lock`. `deno_runtime` re-exports `deno_node`,
   so prefer `deno_runtime::deno_node` over a direct `deno_node` pin.
2. **Reconcile the seam.** Build; fix `node_services.rs` / `resolver.rs` for any
   trait/struct signature changes (`NodeRequireLoader`,
   `NpmPackageFolderResolver`, `InNpmPackageChecker`, `NodeExtInitServices`,
   `NodeResolver::new`, `PackageJsonResolver`, `NodeResolutionSys`,
   `CjsCodeAnalyzer`, `NodeCodeTranslator`; `deno_graph::source::{Loader,
   Resolver}`, `GlobalHttpCache`).
3. **Reconcile the snapshot.** Fix `build.rs` for `create_runtime_snapshot` /
   `SnapshotOptions` / `LazyExtensionFileKind` changes, and re-check the
   `TS_VERSION` constant.
4. **Update `crates/inka-runtime/runtime-version`** (base + revision).
5. **Build and install the tuple**, then run the contract matrix
   (`scripts/ci/runtime-matrix.sh`).
6. **Run the release smoke** (`scripts/ci/smoke.sh`) against a staged release.
7. If the engine now provides a new capability, advance the `inka build`
   `runtime>=` floor so artifacts that need it don't run on an older tuple.

## Contract matrix

`scripts/ci/runtime-matrix.sh` is the update gate. It installs packages into a
throwaway project's `node_modules` and checks, offline:

- CJS `require()` of a leaf (`ms`), a wrapper (`ws`), and a nested dep (`debug`),
- `require("node:path")`,
- ESM `import` of CJS with named + default exports (`ws`, `debug`),
- `require()` of ESM (`effect`),
- circular `require()`,
- a `deno.json` import map → `jsr:@std/assert` (offline Deno cache),
- the release-package ESM matrix (`effect`, `hono`, `ws`, `node:vm`),
- native `.node` addons (deny without `ffi`; load with `--allow-sys --allow-ffi`).

A failure localizes to the seam. Run it on any PR that touches
`crates/inka-runtime`, `crates/inka-bundler`, the Deno pins, or `runtime-version`.

## Capability

Native CJS is a runtime capability. `inka build` embeds a `runtime>=0.266.3`
floor by default, so artifacts always select a tuple that can run raw CommonJS
packages from `node_modules`/the Deno cache. A tuple bump that changes this
capability must bump the default floor in `crates/inka/src/build.rs` and
`crates/inka-runtime/runtime-version` together.

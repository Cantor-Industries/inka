# Runtime tuple updates

The inka engine is a `cdylib` built against a specific Deno release. The Deno
crates (`deno_runtime`, `deno_core`, `deno_node`, `node_resolver`, …) are **not a
stable public API**: every cross-crate type must match the Deno tag's own
`Cargo.lock`. A tuple update is therefore a deliberate, tested change, not a
routine dependency bump.

A tuple is `libinka_runtime-<v>.so`, where `<v>` is the pinned `deno_runtime`
base (`0.xxx.0`) plus an inka revision (`.1`, `.2`, …), tracked in
`crates/inka-runtime/runtime-version`. Behavior-only changes (no Deno bump)
advance the revision; a Deno bump moves the base and restarts the revision.

## The seam

All Deno API surface is confined to two places so a bump stays contained:

- `crates/inka-runtime/src/node_services.rs` — the only file that names
  `deno_node` / `node_resolver` / `deno_core` types (the CJS/node-services seam),
- `crates/inka-runtime/build.rs` — the V8 snapshot + residual lazy sources.

Everything else (`lib.rs`, the CLI) works through our own types.

## Bump procedure

1. **Move the pins together.** Update `crates/inka-runtime/Cargo.toml`
   (`deno_core`, `deno_runtime`, `deno_node` if named, `node_resolver`,
   `deno_semver`, `deno_error`, `sys_traits`, `deno_ast`) to the exact versions
   from the target Deno tag's `Cargo.lock`. `deno_runtime` re-exports
   `deno_node`, so prefer `deno_runtime::deno_node` over a direct `deno_node`
   pin to avoid version skew.
2. **Reconcile the seam.** Build; fix `node_services.rs` for any trait/struct
   signature changes (`NodeRequireLoader`, `NpmPackageFolderResolver`,
   `InNpmPackageChecker`, `NodeExtInitServices`, `NodeResolver::new`,
   `PackageJsonResolver`, `NodeResolutionSys`, `CjsCodeAnalyzer`,
   `NodeModuleExportAnalyzer`, `NodeCodeTranslator`).
3. **Reconcile the snapshot.** Fix `build.rs` for `create_runtime_snapshot` /
   `SnapshotOptions` / `LazyExtensionFileKind` changes, and re-check the
   `TS_VERSION` constant (the bundled TypeScript compiler version).
4. **Update `crates/inka-runtime/runtime-version`** (base + revision).
5. **Build and install the tuple**, then run the contract matrix
   (`scripts/ci/runtime-matrix.sh`).
6. **Run the release smoke** (`scripts/ci/smoke.sh`) against a staged release.
7. If the engine now provides a new capability, advance the `inka build`
   `runtime>=` floor so artifacts that need it don't run on an older tuple.

## Contract matrix

`scripts/ci/runtime-matrix.sh` is the update gate. It builds a throwaway store
(`ms`, `ws`, `debug`) and checks, offline:

- CJS `require()` of a leaf (`ms`), a wrapper (`ws`), and a nested dep (`debug`),
- `require("node:path")`,
- ESM `import` of CJS with named + default exports (`ws`, `debug`),
- `require()` of ESM (`effect`),
- circular `require()`,
- the patched-store ESM matrix (`effect`, `hono`, `ws`, `@std/assert`, `node:vm`).

A failure localizes to the seam. Run it on any PR that touches
`crates/inka-runtime`, the Deno pins, or `runtime-version`.

## Capability

Native CJS is a runtime capability. `inka build` embeds a `runtime>=0.266.2`
floor by default, so artifacts always select a tuple that can run the raw
CommonJS packages shipped in the store and `vendored/`. A tuple bump that
changes this capability must bump the default floor in `crates/inka/src/build.rs`
and `crates/inka-runtime/runtime-version` together.

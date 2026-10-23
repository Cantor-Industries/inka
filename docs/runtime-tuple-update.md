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

A **beta** engine build uses the same scheme with a prerelease suffix
(`0.267.2-beta.1`, ordered below the final `0.267.2`). It is published only in a
prerelease; a beta toolchain (or `--beta`/`INKA_CHANNEL=beta`) installs and
selects it, while a stable toolchain never does. At stable release the
revision is published without the suffix (`0.267.2`), which supersedes every
beta of that base.

## Finding the target tuple

Never resolve the Deno crates with "latest compatible": the source of truth is the
target Deno tag's own `Cargo.lock`. A convenient workflow:

```sh
# 1. Newest published engine crate.
curl -s -A inka https://crates.io/api/v1/crates/deno_runtime | jq -r .crate.max_version

# 2. Newest Deno releases (the engine crate publishes with the tag).
curl -s 'https://api.github.com/repos/denoland/deno/releases?per_page=5' \
  | jq -r '.[] | "\(.tag_name)  \(.published_at[:10])"'

# 3. Pull that tag's lockfile and print every pin inka cares about.
curl -sL https://raw.githubusercontent.com/denoland/deno/v2.9.8/Cargo.lock -o /tmp/deno.lock
python3 - /tmp/deno.lock <<'EOF'
import re, sys
lock = open(sys.argv[1]).read()
crates = """deno_runtime deno_core deno_ast deno_error deno_graph deno_config
deno_cache_dir deno_resolver deno_semver deno_terminal deno_media_type
import_map node_resolver oxc_resolver v8""".split()
for c in crates:
    m = re.search(r'name = "%s"\nversion = "([^"]+)"' % c, lock)
    print(f"{c:18} {m.group(1) if m else '-'}")
EOF
```

Confirm the mapping by checking that the tag's lock contains the
`deno_runtime` version from step 1 (`0.268.0` ↔ `v2.9.8`). Then choose the tuple
number:

- behavior-only change, same base → revision +1 (`0.268.1` → `0.268.2`);
- Deno base moves → new base, revision restarts at `.1` (`0.267.2` → `0.268.1`);
- prerelease → append `-beta.N` and publish it on the beta channel only.

## Seam audit before building

Most bumps are source-neutral. Check what changed in the code paths inka
compiles against *before* paying for a full rebuild:

```sh
curl -s 'https://api.github.com/repos/denoland/deno/compare/v2.9.7...v2.9.8' \
  | jq -r '.files[].filename' \
  | grep -E '^(libs/(core|node_resolver|resolver|config|cache_dir)/|runtime/|ext/node/|cli/rt/)'
```

Read the result as:

- only `Cargo.toml` version lines → source-neutral; the compile is the check;
- source changes under `libs/core`, `runtime/`, `libs/node_resolver`, or
  `ext/node` → expect work in the seam files listed below;
- changes to `cli/rt/desktop.rs`, `cli/rt/hmr.rs`, or `cli/tools/framework.rs` →
  re-sync the vendored copies (`desktop_js.rs`, `desktop_api.rs`, `hmr.rs`,
  `framework.rs`) and update `scripts/ci/desktop-js-pins.sh`.

Two pins are not visible in that diff and must be checked by hand:

- **V8** — the target lock's `v8` version. A change means a new prebuilt archive
  (and a longer first build); an unchanged version means a plain rebuild.
- **TypeScript** — `cli/tsc/00_typescript.js` contains `version = "6.0.3"`.
  Update `TS_VERSION` in `crates/inka-runtime/build.rs` when it moves.

`deno_graph`, `deno_ast`, and the other standalone crates live in their own
repos: read the release notes for the target version (for 0.111.1 it was only a
dependency/TLS-root update).

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
   `deno_cache_dir`, `deno_config`, `deno_resolver`, `import_map`) to the
   versions from the target Deno tag's `Cargo.lock`. `deno_runtime` re-exports
   `deno_node`, so prefer `deno_runtime::deno_node` over a direct `deno_node`
   pin.
2. **Update the lockfile without collateral churn.** With the pins edited, update
   only the changed crates — a bare `cargo update` also bumps `serde`, `url`,
   etc.:

   ```sh
   for spec in deno_runtime:0.268.0 deno_core:0.413.0 deno_cache_dir:0.52.0 \
               deno_config:0.110.0 deno_graph:0.111.1 deno_resolver:0.91.0 \
               node_resolver:0.98.0; do
     cargo update -p "${spec%%:*}" --precise "${spec##*:}"
   done
   ```

   Then assert there is exactly one copy of each shared crate. Two versions of
   e.g. `deno_graph` or `node_resolver` mean two incompatible trait universes,
   which surfaces later as an opaque type error:

   ```sh
   python3 - <<'EOF'
   import re, collections
   lock = open("Cargo.lock").read()
   versions = collections.defaultdict(set)
   for m in re.finditer(r'\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]+)"', lock):
       versions[m.group(1)].add(m.group(2))
   dup = {k: v for k, v in versions.items()
          if len(v) > 1 and (k.startswith("deno") or k in
                             ("node_resolver", "import_map", "sys_traits"))}
   print(f"duplicate versions: {dup}" if dup else "no duplicate Deno versions")
   EOF
   ```

   (`cargo tree -d` also lists the same version twice when it appears with
   different feature sets or targets, so prefer the lockfile check.)

3. **Reconcile the seam.** Build; fix `node_services.rs` / `resolver.rs` for any
   trait/struct signature changes (`NodeRequireLoader`,
   `NpmPackageFolderResolver`, `InNpmPackageChecker`, `NodeExtInitServices`,
   `NodeResolver::new`, `PackageJsonResolver`, `NodeResolutionSys`,
   `CjsCodeAnalyzer`, `NodeCodeTranslator`; `deno_graph::source::{Loader,
   Resolver}`, `GlobalHttpCache`).
4. **Reconcile the snapshot.** Fix `build.rs` for `create_runtime_snapshot` /
   `SnapshotOptions` / `LazyExtensionFileKind` changes, and re-check the
   `TS_VERSION` constant.
5. **Update `crates/inka-runtime/runtime-version`** (base + revision) and the
   drift constants: `VENDORED_DENO_RUNTIME` in
   `scripts/ci/desktop-js-pins.sh`, plus the version comments in `build.rs`,
   `desktop_api.rs`, `desktop_js.rs`, `hmr.rs`, `framework.rs`, the runtime
   README, and `docs/release-process.md`.
6. **Build and verify locally** with the recipe below, then run the contract
   matrix (`scripts/ci/runtime-matrix.sh`).
7. **Run the release smoke** (`scripts/ci/smoke.sh`) against a staged release.
8. If the engine now provides a new capability, advance the `inka build`
   `runtime>=` floor so artifacts that need it don't run on an older tuple.

## Local build and verification

The engine is the heavy part. Put the target directory on a roomy disk (a cold
release build needs roughly 25–30 GB) and point `LIBCLANG_PATH` at a `libclang`
for the runtime's `desktop` feature (laufey uses `bindgen`). `CARGO_HOME` can
stay default; the registry and V8 prebuilt are reused.

```sh
export CARGO_TARGET_DIR=/path/to/roomy/disk/inka-target
export LIBCLANG_PATH=/usr/lib/llvm-18/lib   # only for --features desktop

# Fast correctness check first; it compiles the whole Deno tree in check mode.
cargo check -p inka-runtime --features desktop

# The same artifacts CI builds (runtime-ci.yml).
cargo build --locked --release -p inka --features bundle
cargo build --locked --release -p inka-launcher
cargo build --locked --release -p inka-desktop-shim
cargo build --locked --release -p inka-runtime --features desktop

# Gates.
scripts/ci/deno-pins.sh
scripts/ci/desktop-js-pins.sh
scripts/ci/abi-symbols.sh "$CARGO_TARGET_DIR/release/libinka_runtime.so"
cargo test  --locked --release -p inka-runtime --features desktop --lib
cargo clippy --locked -p inka-runtime --features desktop --all-targets -- -D warnings

# Stage the tuple for the contract matrix, and warm the jsr cache it expects.
rt=$(tr -d '[:space:]' < crates/inka-runtime/runtime-version)
export INKA_RUNTIME_HOME=/tmp/inka-runtime DENO_DIR=/tmp/inka-deno
mkdir -p "$INKA_RUNTIME_HOME" "$DENO_DIR"
cp "$CARGO_TARGET_DIR/release/libinka_runtime.so" \
   "$INKA_RUNTIME_HOME/libinka_runtime-$rt.so"
printf 'import "jsr:@std/assert@1";\n' > /tmp/warm.ts
"$CARGO_TARGET_DIR/release/inka" cache /tmp/warm.ts
export INKA_LAUNCHER="$CARGO_TARGET_DIR/release/inka-launcher"
bash scripts/ci/runtime-matrix.sh "$CARGO_TARGET_DIR/release/inka"
```

Install the tuple and confirm selection. A `-beta.*` tuple is only selected in
the beta channel (`INKA_CHANNEL=beta`); a stable tuple is selected by a stable
toolchain automatically.

```sh
install -m755 "$CARGO_TARGET_DIR/release/libinka_runtime.so" \
  ~/.local/share/inka/runtime/libinka_runtime-$rt.so
inka doctor          # newest tuple with a ✓ is the one `run`/`build` will load
inka run /tmp/hello.ts
```

Keep the previous tuple installed until the new one passes: it is the rollback
path, and artifacts roll forward to the newest tuple that satisfies their
manifest.

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

Engine capabilities are named, advertised by the runtime
(`inka_runtime_features()`), and required by artifacts (`requires=` in the
manifest). `crates/inka-format` owns the feature list and each feature's
minimum tuple (`FEATURE_FLOORS`) plus the `SECURITY_FLOOR` (the tuple that
guarantees deny-by-default, realpath confinement, and the `_dir` entry):
`inka build` computes the floor as the maximum of the security floor and the
required features' floors. The launcher verifies `requires=` against the
runtime's advertised set when the symbol exists.

When a tuple bump adds a capability an artifact may need, add it to
`FEATURE_FLOORS` (and to the runtime's advertised set) and emit `requires=` from
`crates/inka/src/build.rs`. When the *security* model changes, bump
`SECURITY_FLOOR`. Keep `crates/inka-runtime/runtime-version` in step.

## Worked example — 0.267.0 → 0.268.0 (Deno 2.9.7 → 2.9.8)

`deno_runtime 0.268.0` published 2026-10-21, the same day as Deno `v2.9.8`.
Extracting the tag's lockfile gave:

| Crate | From | To |
|---|---|---|
| `deno_runtime` | 0.267.0 | 0.268.0 |
| `deno_core` | 0.412.0 | 0.413.0 |
| `deno_cache_dir` | 0.51.0 | 0.52.0 |
| `deno_config` | 0.109.0 | 0.110.0 |
| `deno_graph` | 0.111.0 | 0.111.1 |
| `deno_resolver` | 0.90.0 | 0.91.0 |
| `node_resolver` | 0.97.0 | 0.98.0 |
| `deno_ast`, `deno_error`, `deno_semver`, `deno_terminal`, `deno_media_type`, `import_map`, `sys_traits`, `oxc_resolver` | — | unchanged |
| V8 / TypeScript | 150.4.0 / 6.0.3 | unchanged |

The tag-to-tag diff touched only `Cargo.toml` files in `libs/node_resolver`,
`libs/resolver`, `libs/config`, and `libs/cache_dir`; the `libs/core` changes
were internal (op-context lifetime, `JsRealmInner::destroy` returning
`Rc<OpCtxs>`, neither used by inka); the vendored `cli/rt/desktop.rs` and
`cli/rt/hmr.rs` were byte-identical. `deno_graph 0.111.1` was a dependency and
TLS-root update. Result: **source-neutral** — the pins, the tuple
(`0.267.2` → `0.268.1`), and the version comments were the whole change; the
compile and the gates passed on the first try.

Use this as the baseline expectation for a patch release. A minor Deno bump is
where trait and snapshot changes are likely, and where the seam section earns
its keep.


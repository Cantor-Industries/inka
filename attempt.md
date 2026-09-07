# Phase 2 attempt — vendored built-in packages, offline npm/jsr, code caches

Status: **restart from scratch.** The repo on `master` is reset to before Phase 2
(commit `8c467cc`, end of Phase 1 / multi-file artifacts). All Phase-2 work is
preserved for reference on the branch **`phase2-experiments`**.

This file is the complete retrospective + restart brief. Read it before writing
any Phase-2 code. It exists because Phase 2 was attempted three different ways
and each stalled on the same underlying truth; the point of this file is to stop
re-learning it the hard way.

---

## 1. TL;DR

- Goal: packages that "come with the runtime" (vendored, zero-install) **and**
  standard npm/jsr for everything else, **offline-first** at runtime, with
  pre-warmed (non-embedded) code caches.
- Root cause of every stall: **"npm/jsr resolution" is the entire Deno CLI
  module-loader**. It is not a published library seam. The only maintained
  embeddable wrapper is `libdeno`, whose internals are private and whose
  behavior is exactly "deno the CLI": resolve against a workspace cwd, contact
  the registry to pick versions, materialize `node_modules` on demand.
- Three attempts: (1) port libdeno's internals (~5–6k lines) → too big;
  (2) depend on/vendor `libdeno` and treat its semantics as "standard deno"
  → offline-first + no-install contradicted those semantics; (3) "deep-patch"
  libdeno for true offline → discovered the load-bearing invariant: **jsr
  version selection asks the registry even when the bytes are cached**, and
  libdeno has no cache-only switch for that path.
- The proposed way forward (still unproven, probe first): a **lockfile-pinned
  store** so resolved snapshots come from a `deno.lock`, never the registry.

## 2. Original goals & decisions locked with the user (Phase-2 brief)

- **Vendored built-ins** = a curated set that ships with the runtime install
  (`~/.inka-runtime/pkg/<deno-version>/`), imported by normal `npm:`/`jsr:`
  names, zero-install, **not embedded inside the .so**.
- **Everything else** uses standard npm/jsr, but **offline-first/pre-warm only**:
  the runtime never fetches; packages must be installed into the store ahead of
  time by a tool.
- **Code caches**: signed, pre-warmed, non-embedded (parse-free loads).
  Cross-process RAM sharing of compiled JS was explicitly **dropped** (only the
  startup-snapshot path could do that, which meant embedding — rejected).
- Data-driven seed. Initial: `jsr:@std/assert`, `jsr:@std/path`,
  `jsr:@std/async`, `npm:zod`.
- Sequencing: 2a (store + offline npm/jsr) then 2b (code caches).
- Trust: SHA-256 manifests now; signatures later.

## 3. State at the start of Phase 2 (what Phase 1 left us)

- Thin artifact = Rust launcher (~355 KB) embedding source/manifest (trailer
  v1 `INKFOOT2`, v2 `INKFOOT3` multi-file archive) + a shared runtime `.so`.
- Runtime was a hand-rolled cdylib over `deno_runtime 0.266.0` (`crates/
  runtime-deno`): MainWorker + snapshot build script + custom `ModuleLoader`
  + permissions (deny-by-default, additive `_perm`/`_dir` ABI symbols).
- `inka build` (closure / `--embed-dir`), `inka install`, checksums, TS single +
  multi-file, `INKA_DEBUG`.

## 4. Chronology of the attempt (condensed)

1. Planned a custom `inka-store` crate: adopt `deno_graph`/`deno_npm*` to build
   the loader. Version-alignment research against the deno repo tag for
   `deno_runtime 0.266.0` = **deno v2.9.6** (full pin list in §9).
2. Read libdeno's `module_loader.rs`/`services.rs`/`permissions.rs` closely.
   Realized the loader depends on ~5–6k lines of **private** infrastructure
   (resolver factories, graph resolver, npm installer, file fetcher, node CJS
   analysis, caches). Port was impractical → **user chose: vendor libdeno and
   depend on it** (replacing our hand-rolled engine).
3. Pivot executed on commit `ed67e41`:
   - `crates/runtime-deno` became a thin cdylib calling `libdeno::run`;
     removed our custom engine, snapshot build, `inka-store`.
   - Vendored `vendor/libdeno` (deno 2.9.5 / deno_runtime 0.265.0, MIT),
     trimmed to build files.
   - Patched its `permissions.rs` to add `--deny-*` and deny-by-default
     (upstream libdeno treats empty-flags+no-prompt as a construction error).
   - Tuple relabeled `0.265.0`. Baseline re-verified (single/multi JS+TS,
     dir-mode dynamic import, permissions).
4. 2a store work (`46251d1`):
   - `inka pkg seed|install|list` (shells to the `deno` CLI; network only here);
   - launcher points `DENO_DIR` and a store `proj/` at the engine;
   - runtime: store `proj` as resolution cwd, auto-grants store reads +
     `--allow-import` for the registries;
   - **verified offline** (dead-proxy trick, §7) for `npm:zod` (BYONM
     `node_modules` in `proj/`) and `jsr:@std/assert` (DENO_DIR cache).
5. 2b code caches: libdeno's disk cache covers **script/eval contexts only**;
   **ES-module** code caching rides a `deno_core` `ModuleLoader` seam libdeno
   deliberately leaves unimplemented (see its `limits.rs` comment). Parked.

## 5. Concrete findings you must not re-discover the hard way

- **libdeno == deno CLI semantics.** It resolves against a workspace cwd,
  contacts registries for version selection, and its npm resolver is
  *managed*: it writes `node_modules/` into the resolution base on demand.
  `DENO_DIR` is a byte cache, not a resolution source of truth.
- **`allow_remote: false` is the WRONG offline knob.** It surfaces as
  `--no-remote` and blocks **cached** jsr reads too, because jsr version
  resolution always requests `https://jsr.io/<pkg>/meta.json` regardless of
  cache state. Correct model: `allow_remote: true` + `CacheSetting::Use`
  (cache-first; fetch only on a true miss).
- **Permissions that bit us (all patched in the vendor):**
  - deny-by-default impossible upstream (empty flags = construction error);
    no `--deny-*` support at all.
  - the engine's own module reads are read-permission-gated → implicit
    `--allow-read` of the staged tree (and store dirs) required.
  - cached jsr/npm reads are import-permission gated → auto-grant
    `--allow-import=jsr.io` / `registry.npmjs.org` when a store is active.
- **Staged module specifiers must be STABLE across runs** for any code cache to
  hit. Our staging used pid-based temp paths → cache keys changed every run.
  (Would matter if 2b is ever attempted again.)
- **jsr offline needs exact version pinning** (or an import map) — unpinned
  `jsr:@std/assert` requires the registry to resolve a version.
- **npm offline works** via bring-your-own-node_modules: a `node_modules/`
  present in the resolution base (store `proj/`) resolves without the registry.
- **deno CLI will not produce `node_modules` for you** by default (Deno 2 only
  materializes it when a `package.json`/BYONM setup exists); libdeno creates it
  on first managed run in its resolution cwd. That first run needs network.
- Version-alignment: deno crates are not a stable public API; every
  cross-crate type must match the deno repo tag's own `Cargo.lock` exactly
  (see §9). libdeno 0.3.2 ↔ deno 2.9.5; our original runtime was 2.9.6/0.266.
- Rebuild cost: runtime `.so` ≈ **101 MB**, single-engine rebuild ~5–6 min
  (deps cached), first build ~12–20 min. Use the big-disk cargo home (§7).
  Only the one crate changed ⇒ fast-ish, but every iteration is a 5-min loop;
  **run the decisive probe before building around an assumption.**

## 6. Why it stalled (honest)

1. The chosen mechanism (deno-CLI semantics via libdeno) contradicts the target
   model's invariants (offline-first, no installs, version-locked store,
   cross-process parse-free). I patched symptoms (permissions, `allow_remote`,
   node_modules location) instead of confronting the load-bearing one: version
   selection wants the registry even when bytes are cached.
2. Process error: I built 2a's structure around an untested assumption (warm
   `DENO_DIR` ⇒ offline) instead of probing with a network cut first.
3. The remaining work is architectural (lockfile-pinned resolution), not a
   tweak; and 2b requires V8-module-code-cache internals libdeno sidestepped.

## 7. Environment / commands cheat-sheet

- Repo: `/home/kook/inka`. Branch `master` (reset to `8c467cc`); Phase-2 work on
  `phase2-experiments`; future attempts on **`phase2-rework`** (this file lives
  here).
- Heavy cargo builds (inka-runtime / vendored libdeno) must use the big disk:
  ```
  export CARGO_HOME=/media/kook/641ee182-ef10-4fc8-96b8-2de6f780603f/inka-cargo-home
  export CARGO_TARGET_DIR=/media/kook/641ee182-ef10-4fc8-96b8-2de6f780603f/inka-build/target
  cargo build --release -p inka-runtime
  ```
  Then install the tuple:
  ```
  cp $CARGO_TARGET_DIR/release/libinka_runtime.so ~/.inka-runtime/libinka_runtime-0.265.0.so
  ```
  (inka-launcher/inka build with default env; fast.)
- deno CLI for pre-warm: `~/.deno/bin/deno` (2.9.6); use `DENO_BIN` or PATH.
- **Offline simulation that actually works:** a dead proxy makes every network
  attempt fail instantly and loudly:
  ```
  HTTPS_PROXY=http://127.0.0.1:1 HTTP_PROXY=http://127.0.0.1:1 ALL_PROXY=http://127.0.0.1:1 ./app
  ```
- Store that was produced (for reference on the branch):
  `~/.inka-runtime/pkg/0.265.0/{deno,proj}` where `deno/` is a DENO_DIR and
  `proj/` holds `node_modules/` (BYONM). Launcher sets `DENO_DIR` and
  `INKA_PKG_PROJ`; runtime uses store `proj` as resolution cwd.
- Re-measured startup is ~0.2 s dominated by engine boot, which swamps parse
  cost at this scale — relevant if 2b is ever reconsidered.

## 8. Restart brief (what to do differently)

**Probe first; commit code only after the probe answers the invariant.**

M0 probe (scratch dirs, dead-proxy):
- Does a lockfile project (`deno.json` + `deno.lock`, generated by
  `deno install --lock-write` or `deno cache` with lockfile) let a fresh
  process resolve offline:
  - `npm:zod` unpinned (BYONM node_modules + lock),
  - `jsr:@std/assert@<exact>` pinned + subpath,
  - `jsr:@std/assert` unpinned (hypothesis: fails → needs exact pin / map)?
- Does libdeno honor the `deno.lock` in its resolution cwd (check vendored
  code + behavior), and what is the minimal store-project shape it accepts?

M1 store generation: `inka pkg seed/install` writes
`pkg/<ver>/proj/{deno.json, deno.lock, node_modules}` + `DENO_DIR` +
`MANIFEST.sha256`, using the deno CLI (network at install time only).

M2 runtime enforcement: resolution cwd = store project; failures that would
require the registry become a clean `run "inka pkg install <spec>"` error; no
network attempted. Keep deny-by-default + auto store grants.

M3 verification: full dead-proxy matrix + regressions; commit per milestone.

Policy to confirm with the user in the fresh chat:
- jsr imports in artifacts must be **exact-pinned** (recommended) — unpinned
  jsr is a clear error, not a fetch; or invest in import-map injection later.
- Optional hard egress safety net during store-mode runs (dead `HTTPS_PROXY`
  injection) as belt-and-braces.
- `inka pkg` keeps the `deno` CLI + network at install time (yes) and freezes
  added versions into the store lock (recommended).

## 9. Appendix

### Deno crate pins at deno v2.9.6 (Cargo.lock of denoland/deno tag v2.9.6)
`deno_core 0.411.0`, `deno_runtime 0.266.0`, `deno_graph 0.111.0`,
`deno_resolver 0.89.0`, `deno_npm_installer 0.53.0`, `deno_npm_cache 0.77.0`,
`deno_npm 0.70.0`, `deno_cache_dir 0.50.0`, `deno_config 0.108.0`,
`node_resolver 0.96.0`, `deno_semver 0.10.1`, `deno_error 0.7.1`,
`deno_ast 0.53.3`, `deno_fs 0.168.0`, `deno_features 0.55.0`,
`deno_npmrc 0.19.0`, `deno_media_type 0.4.0`, `tokio 1.47.1`.
Note: `deno_graph`, `deno_npm_cache`, `deno_npm_installer` are NOT transitive
deps of `deno_runtime` (they live in the CLI) — adding them requires exact pins.
libdeno 0.3.2 pins the one-back line: deno 2.9.5 / deno_runtime 0.265.0 /
deno_core 0.410.0 / deno_resolver 0.88.0 / deno_graph 0.110.1.

### Permission DSL → libdeno flags (worked mapping)
`permissions=all` → `allow_all_permissions`; `permissions=none`/empty → empty
flags (deny-by-default in patched libdeno); `allow-<cat>=…` → `--allow-<cat>[=list]`;
`deny-<cat>=…` → `--deny-<cat>[=list]`; `*` → no value. Runtime adds implicit
`--allow-read` of the staged tree, and (store active) store dirs +
`--allow-import=jsr.io` + `--allow-import=registry.npmjs.org`.

### Vendored-libdeno patches made (reference on phase2-experiments)
- `permissions.rs`: deny flags, allow-all trimmed by denies, deny-by-default.
- `services.rs`: `PermissionedFileFetcherOptions { allow_remote: true, cache_setting: CacheSetting::Use }`
  (cache-first; comment explains why `false` breaks cached jsr).

### The last proposed but unimplemented direction (Option A)
Lockfile-pinned store so resolution never consults a registry (see §8). This
file's goal is that the fresh chat starts here rather than at "build the
loader again from scratch."

## 10. Resolution landed: the self-contained tar store (supersedes §8)

Implemented on `phase2-rework` (commits `9d58731`…`84c7def`), replacing the §8
libdeno/CLI-resolution restart brief. Unlike the failed phase-2 pivot, this does
**not** inherit Deno-CLI resolution — the engine is a hand-rolled loader and the
store is the only source of truth, so the "asks the registry even when cached"
invariant cannot arise.

### Mechanism
- Store root default `$INKA_STORE`, else `~/.inka-runtime/store`; launcher sets
  it for you when a `store/` dir sits next to the resolved `.so`.
- Layout: `store/packages/<npm-name>/<version>/node_modules/<npm-name>/…` — one
  **self-contained closure** per installed version (the real package-manager
  resolution of that package, hoisted deps included).
- jsr rides jsr's npm-mirror identity: `jsr:@scope/name` → `@jsr/scope__name`.
- Distribution = tarballs that are already built: `inka pkg tar <spec>…`
  (network-only) runs `npm install` once and tars the closure, so pre/postinstall
  already executed when the tar was made. Consumers (runtime installs, `inka pkg
  seed`) only download → sha256-verify → extract; nothing installs or runs on
  the machine. `inka install` seeds a `<release>/store/` payload when present.
- Runtime loader (`PkgLoader` in `crates/inka-runtime`) dispatches `npm:`/`jsr:`
  to the store, bare imports from inside the store via a node_modules walk,
  serves file/relative/node: as before, and rejects `http(s):` outright.

### Version policy
Exact specifiers recommended. Unpinned/range imports resolve only when the
store has a unique (or best, for ranges) satisfying version; absent → clean
`run "inka pkg seed"` error. Never a network call from the engine.

### Verified (dead `HTTPS_PROXY`)
`npm:zod`, `jsr:@std/assert@1.0.0`, jsr subpath (`/assert`), transitive bare
`@jsr/std__internal`, single- and multi-file, exact/range/unpinned/ambiguous and
missing-version error paths, `node:vm` intact, permission paths, http-import
rejection, and the full `pkg tar` → `pkg seed` / `install`-payload →
artifact-run flow into a fresh `--home`.

### Known limits
CJS/`require()` dependencies not served (loader picks import/default conditions
only); curated set is pure-ESM. Store updates are additive — installing a newer
version leaves the older one in place (unpinned imports then become ambiguous by
design). No authenticity signing yet (sha256 integrity only).

### Bare resolution (post-landing refinement)
Bare specifiers now resolve with no `npm:`/`jsr:` prefix: `import { z } from "zod"`,
`import { assertEquals } from "@std/assert"` (scoped bare names try the npm identity
first, then the jsr-mirror identity `@jsr/scope__name`). Node built-ins are also
importable bare (`vm` ≡ `node:vm`, `process` ≡ `node:process`, incl. `name/sub`
aliases like `fs/promises`); Node semantics apply — core wins over node_modules.
`npm:`/`jsr:` + `@version` remain the way to pin an exact version. The `inka build`
warning for non-local imports was removed (they're real at run time now).

### --transpile for multi-file (Deno-style precompile)
`inka build --transpile` now works for multi-file apps: each `.ts/.mts/.cts`
module is transpiled to JS at build time but keeps its original archive path (no
import rewriting). Such artifacts carry a new trailer magic `INKFOOT4`; the
launcher sets `INKA_PRECOMPILED=1` and the runtime serves those modules as plain
JS (skips `maybe_transpile_source`). Runtime-transpile artifacts (default) still
use `INKFOOT3`. JSX/TSX entries with `--transpile` error (unsupported).

### Store redesign: shared pool + manifest-driven snapshot (supersedes §10 closure model)
The store is now ONE hoisted `node_modules` pool (classic npm layout): independent
packages may pin different versions of a shared dep (npm hoists + nests), and
Effect-family packages share a single `effect` when versions align. Distribution is
a whole-store snapshot tar (`store.tar.gz`): `inka pkg snapshot` npm-installs the
seed set together (pre/postinstall baked there); `pkg seed` / `inka install` verify
+ atomically replace `store/node_modules`. The seed set is no longer hard-coded: a
default `seed-manifest.json` ships with inka (repo root; discovery order
`--seed-manifest` → `$INKA_SEED_MANIFEST` → `./seed-manifest.json` → next to the
binary); users swap it to curate their own store. Default set: `zod@3.23.0`,
`jsr:@std/assert@1.0.0`, `effect@3.22.1`, `@effect/platform@0.97.1`,
`@effect/platform-node@0.108.1` (+ auto peers). The loader now resolves a package
root straight from `store/node_modules/<identity>` and checks pinned imports
against the hoisted copy's version. Verified: `effect` core runs offline; the
Effect `platform`/`platform-node` tiers still need engine Node compat (CJS `ws`
interop, `process.env` reads at import) that isn't implemented.

### Engine/resolver split (fast iteration for import policy)
`crates/inka-runtime` (heavy: V8 + snapshot + deno_runtime) now contains no
import-resolution logic. All of it moved to a new pure-Rust cdylib
`crates/inka-resolver` (`libinka_resolver-<v>.so`, no deno/V8 dep) exposing a
stable C ABI: `inka_resolver_abi()==1`, `inka_resolver_resolve(store, referrer,
specifier)` -> `{UseDefault, File, Builtin(node:), Error}`, `inka_resolver_free`.
The engine dlopens it once per process from `$INKA_RESOLVER` (launcher defaults
it to the newest installed `libinka_resolver-*.so`); on a miss it degrades to
relative/file/node:/data: + offline rejection only. Resolver changes rebuild in
~1s with no engine rebuild (verified: message change took effect immediately).
Versioning: the resolver is its own tuple installed alongside the runtime
(`inka install` ships `libinka_resolver-1.0.0.so`; `inka list` shows both).
Known correctness catch: serde_json maps sort keys, so `exports` conditions must
be chosen by a fixed import>node>default priority, never by file order (this
bit us when `effect` resolved to its CJS `default` build).

## 11. CommonJS support — design risks & future work (Part 2)

Status: **probe/design open**. We chose a *custom bounded CJS loader* over adopting
`deno_runtime` node services wholesale (Option B). This section records the risks
of that path and the open design questions a future session should resolve before
(or while) building it.

### Why CJS exists at all
`@effect/platform-node` (via `ws`, and `undici`/native-binding fallbacks) is
CommonJS. Node lets ESM `import` a CJS module by executing it with
`module/exports/require` and synthesizing `default = module.exports` plus
lexer-detected named exports. Our engine currently serves a CJS file's bytes as an
ESM module, so `import PermessageDeflate from "./lib/permessage-deflate.js"` fails
("does not provide an export named 'default'"). `require()` inside CJS is also
unimplemented.

### The planned (bounded) design — see session notes
- CJS files are NOT handed to V8 as ESM. A tiny JS sidecar (`internal:inka/cjs.js`,
  lazy extension in the snapshot) keeps a per-process module registry
  (`require.cache` semantics) and a synchronous `require()`.
- Resolver (fast crate) classifies a file ESM vs CJS (`.cjs` or `.js` in a package
  without `"type":"module"`), resolves `require()` targets through store/
  node_modules logic, and lexes static named exports.
- Engine `PkgLoader::load()` returns an ESM facade for a CJS file:
  `export default m.exports; export const foo = m.named.foo;` backed by new ops
  (`op_inka_cjs_load`, `op_inka_cjs_require`).

### Risks / known parity gaps of the bounded path
1. **Not full Node CJS.** We accept: no live-binding re-export (named exports are a
   static snapshot), no `require()` of ESM-only packages (ERR_REQUIRE_ESM-style),
   no dynamic `require(expression)`, no `eval`-based loaders, limited
   `cjs-module-lexer` parity (dynamic `module.exports` patterns degrade to
   default-only).
2. **Isolate-integration uncertainty.** Evaluating CJS bodies inside the worker
   isolate and bridging synchronous `require()` to Rust ops must match
   deno_core's module-graph assumptions (error surfaces, top-level await
   restrictions, `this`/globalThis, stack traces, module identity across the
   ESM/CJS boundary, circular requires). There is real risk of subtle
   divergence we will only find by exercising real packages.
3. **One engine rebuild per structural change.** The facade/sidecar/ops live in
   `inka-runtime` (snapshot regeneration + heavy relink), which is exactly the
   slow loop we just split away from. Resolver-only semantics stay fast; anything
   touching the CJS runtime itself is not.
4. **Two sources of module-loading truth.** Store ESM loading stays on
   `PkgLoader`/resolver; CJS loading becomes a parallel path in the engine. That
   duplication is a maintenance and correctness hazard (resolution, confinement,
   caching, and version checks must agree in both).
5. **Scope creep.** "Bounded" tends to grow (package `exports` `require`
   conditions, `node:` builtin requires, `__dirname` in ESM facade,
   `import.meta.dirname`, per-version nested `require` from packages that also
   load ESM). Each addition re-touches the engine crate.
6. **Correctness vs Node divergences** will surface silently (module identity,
   circular refs, getters, `module.exports = function` default interop). Need a
   conformance harness against real packages, not hand tests.
7. **The eventual correct end-state may still be Option B** (deno_runtime node
   services backed by our store) or build-time transformation (bundle CJS to ESM
   at snapshot/seed time). Committing deep into a custom loader could be sunk cost
   if either of those proves cleaner.

### Recommended future study (before/while building)
- Probe whether a *minimal* store-backed `NodeExtInitServices` (npm-folder resolver
  only, no CLI loader) enables deno_node's own `require`/CJS ops — if yes, Option B
  may be far cheaper than a custom loader.
- Investigate build-time CJS→ESM at seed/snapshot time (esbuild-style) as an
  alternative that avoids any runtime CJS; weigh semantics, size, and the
  "pre/postinstall baked at tar time" guarantee.
- Decide module-identity policy when the same package is reached via ESM and CJS.
- Design the conformance test set (ws, undici, msgpackr-extract, a circular-require
  fixture, dual-package hazard) before implementing.
- Keep the resolver as the single source of classification/resolution so any
  future engine-side loader (custom or deno's) can share it.

### M0 probe findings (env panic + CJS) — re-scope of Part 2
With `allow-env` granted the engine panicked; the allow-env crash is a cascade of
missing op-state resources, not one:
- `sys_traits::impls::RealSys` (fixed: injected via a `deno_core::extension!`
  state hook, `inka_rt_state`), then
- `alloc::rc::Rc<dyn deno_node::NodeRequireLoader>` — `msgpackr` (inside
  `@effect/platform`) calls Deno's *own* CJS `require()` ops, which are compiled
  into the snapshot and cannot be bypassed.

Conclusion: for these real packages a *bespoke* CJS loader (Part 2 Option A)
cannot cleanly coexist — `deno_node`'s `require` ops will always be invoked and
must be satisfied. The path that actually unblocks Effect platform is supplying
`WorkerServiceOptions.node_services` = `NodeExtInitServices { node_require_loader,
node_resolver, pkg_json_resolver, sys: RealSys }` backed by our shared store
(deno's node/CJS loader = Option B). That is a sizable, separate milestone with
its own design (see §11); keep the resolver as the policy seam for whatever is
adopted.

## 12. Node services (Option B) milestone plan — deno's own node/CJS loader

Goal: unblock `@effect/platform`/`@effect/platform-node` (env-reading + CommonJS
packages) by giving the engine real deno node services backed by the shared
store, instead of a bespoke CJS loader (probes showed deno's `require()` ops are
compiled into the snapshot and cannot be bypassed).

### Architecture stance
- Keep `crates/inka-resolver` as the *policy seam*: classification (ESM/CJS),
  store/node_modules resolution, jsr mapping, builtins. The engine stays thin.
- Add deno node services to the *engine* for CJS/`require()` only; pure-ESM
  store modules continue to load through `PkgLoader`/resolver.
- Env: deny-by-default remains; env-reading packages need `allow-env` (done).
  The allow-env panic chain is being walked one resource at a time
  (RealSys done via `inka_rt_state`; next is `NodeRequireLoader`).

### Components
1. **Deps (engine):** add `deno_resolver = { version = "=0.89.0", features =
   ["sync"] }` (same pin as deno_runtime). Maybe `deno_node` is reachable via
   deno_runtime re-exports — verify; add direct `deno_node = "=0.196.0"` if not.
2. **Store-backed node resolvers** (replace `NoNpm`/`NoNpmFolder` stubs):
   - `InNpmPackageChecker` → `in_npm_package` = path under store node_modules.
   - `NpmPackageFolderResolver` → map bare name to
     `<store>/node_modules/<name>` (package root), incl. `types_package_folder`.
   - `NodeResolverRc` + `PackageJsonResolverRc<RealSys>` from deno_resolver built
     over those; node modules dir = the store root.
3. **`NodeRequireLoader` impl** (trait in deno_node): `ensure_read_permission`
   (auto-allow reads confined to artifact tree + store, matching our module-read
   model), `load_text_file_lossy` (read file), `is_maybe_cjs(_from_require)` via
   package.json type / extensions (resolver classification), node-module-paths
   walk up the store.
4. **Worker wiring** (`build_services`): construct `NodeExtInitServices {
   node_require_loader, node_resolver, pkg_json_resolver, sys: RealSys }` from
   the store path and pass `node_services = Some(..)` into `WorkerServiceOptions`;
   keep `module_loader = PkgLoader` (ESM) and `NoNpm`/folder stubs removed.
   `npm_process_state_provider` stays None unless a probe needs it.

### Phases (probe-first)
- **P0 probes:**
  (a) CJS-only `require()` chain: seed a tiny store package whose entry is CJS
  and `require()`s another CJS file + a `node:` builtin; run via a small ESM app
  that does nothing but trigger it through a `node:`/`require` path — validates
  the resource set + NodeRequireLoader without ESM interop.
  (b) ESM-import-of-CJS: determine how Deno intends an ESM `import "ws"` (whose
  `wrapper.mjs` default-imports a CJS file) to be served, and whether our
  `PkgLoader` must emit a CJS wrapper module (deno "maybe_cjs" handling) — this
  decides whether P2 reuses deno's machinery or needs our facade.
  (c) `msgpackr` env-read + require chain through `@effect/platform` after P1.
- **P1:** implement components 1–4; require() (from CJS/`require()`) works.
  One heavy engine build; resolver stays unchanged unless classification moves.
- **P2:** ESM↔CJS interop at the loader for store CJS files (decision from P0b).
- **P3:** Effect verification + parity harness (ws, undici, msgpackr-extract,
  circular-require, dual-package hazard) + full regression matrix.

### Open design points (resolve in P0)
- Dual module-identity policy when a package is reached via both ESM and CJS.
- Whether require-reads auto-allow store reads (recommended) vs need read perm.
- Whether deno's ESM loader needs to take over store modules that are CJS
  (loader-level maybe_cjs) — the crux of P2.
- If P2 can't cleanly reuse deno ops for ESM-import-of-CJS, fall back to a
  resolver+engine CJS wrapper emitting `default` + static named exports backed by
  a small CJS registry — but keep it behind the same resolver classification.

### Verify / regressions / docs
Same matrix as prior milestones (bare, builtins, node:vm, --transpile, perms,
spike, install payload) + the new CJS/Effect cases. Update §11 risks + README
limits. Commit per phase. Risks: still bounded, but this milestone is the
largest single engine change; keep resolver decoupled so either path is replaceable.

## 13. Option C / P0 probe results — rolldown-as-crate + post-pass (ws ESM)

Decision context (fresh-session, supersedes the §12 Option-B-as-default stance):
Option C was chosen (ESM-only store + seed-time patches, clean CJS rejection). The
bundler = **rolldown as a Rust crate**, procedural patches at snapshot time, node
deferrals accepted until npm-install is later removed. P0 (probe, all in scratch,
no repo changes) verdict:

### Rolldown crate facts
- `rolldown` 1.2.7 on crates.io (MIT, edition 2024, rustc fine). **Not** semver /
  undocumented / Rust-only issues closed upstream (their own policy) → pin exact
  `=1.2.7`, isolate the dep behind a dedicated crate, golden-test on bumps.
- Rust API is a flat `BundlerOptions` (input/external/platform/format/... single
  struct) + `BundlerBuilder` -> `Bundler`; `bundler.generate().await` ->
  `BundleOutput { assets: Vec<Output> }` (`Output::content_as_bytes()`).
  Required: `platform: Platform::Node`, `external` (natives), `format: Esm`.
- Cold build ~12 min with big-disk cargo home/target (~36k+ lines across the
  rolldown/oxc workspace). Do NOT link into the fast `crates/inka` default build.
- **Deterministic**: two runs of the same bundle are byte-identical.

### The critical finding: rolldown (and esbuild) keep `require()` for externals
Rolldown ESM output of a CJS package externalizes node builtins + optional natives
as a runtime `__require = createRequire(import.meta.url)`. The engine **panics**
on any createRequire (`Rc<dyn deno_node::NodeRequireLoader>` missing in
GothamState), even for builtins — same class as the msgpackr panic. esbuild 0.27.2
output is worse: its ESM `__require` throws "Dynamic require is not supported"
**under real Node** (verified) — rolldown is the correct bundler here.

### Post-pass that makes the bundle engine-viable (validated end-to-end)
Probe `postprocess.py` on the rolldown bundle of ws@8.21.3 `wrapper.mjs`:
1. hoist every `__require("<node builtin>")` into a **default** ESM import
   (`import __rq_x from "node:x"` — default == module.exports, mirrors `require`)
   and rewrite call sites (namespace `import *` broke `class extends EventEmitter`);
2. delete the `import { createRequire } ...` line and redefine `__require` to
   **throw a catchable JS Error** → ws's own try/catch turns the absent optional
   natives (`bufferutil`, `utf-8-validate`) into the pure-JS fallback;
3. neutralize the env knobs read at import time (`process.env.WS_NO_BUFFER_UTIL` /
   `WS_NO_UTF_8_VALIDATE` → `"1"`, which in ws *skips* the native branch) so the
   artifact needs no `allow-env`.

Result: single self-contained ESM (~117 KB, 116,604 B) that under real Node v22
exposes `WebSocket`/`WebSocketServer`/`PerMessageDeflate`/`default` + passes a
127.0.0.1 echo smoke, AND runs in the engine offline — live WebSocketServer +
client echo, `allow-net=127.0.0.1` only, no env, no createRequire, no panic.
Named exports preserved through the whole chain. Resolver's bare-builtin table
already covers every builtin ws touches.

### P3 outcome — @effect/platform-node runs a real HTTP app offline (+ engine parity fix)
A `NodeRuntime.runMain` Effect program now runs a live HTTP round-trip in the engine,
fully offline (`allow-net=*` on a local bind):
`NodeHttpServer.layerTest` (real `node:http` server via engine node builtins) +
`HttpClient` = **NodeHttpClient = undici** (its `Undici.js` loads our patched undici
bundle) -> self-fetch `/ping` returns 200 "pong". This exercises, at runtime:
Effect runtime + layers, NodeContext, node:http/net builtins, `mime` bundle
(httpPlatform), and the **undici** bundle. ws bundle (raw + store echo) and msgpackr
round-trip were already proven; trio proves the whole import graph loads. Scratch
app: /tmp/opencode/inkam0/p3http.

- **Engine parity fix found while running it**: the engine set
  `WorkerOptions.bootstrap.location = Some(main_module)`, which exposes a live
  `globalThis.location` (origin "null" for the staged file:// module). Web code
  that builds URL bases from it breaks — @effect/platform `UrlParams.baseUrl()`
  produced `"null" + pathname`, an invalid `new URL` base ("Invalid URL … with base
  'null/tmp/…/main.js'"). Real `deno run` keeps `globalThis.location` undefined.
  Fixed by leaving `bootstrap.location` unset (inka-runtime lib.rs). One heavy
  engine rebuild; tuple stays 0.266.0.
- **Residual-1 closed — Effect-layered ws echo verified** (`/tmp/opencode/inkam0/p3ws`):
  a `NodeRuntime.runMain` program with an `HttpRouter` `/ws` route upgrades via
  `HttpServerRequest.upgrade` (platform-node's internal upgrade handler calls the
  bundled `ws` `WebSocketServer.handleUpgrade`), echoes through `Socket.writer` +
  `socket.runRaw`, never returning a normal response; a bundle `ws` client connects
  and asserts `echo:ping`. Offline, exit 0. The only real wrinkle: a ws route handler
  must end in `Effect.never` (returning a normal Response writes HTTP bytes over the
  upgraded socket) and must not require the `Socket` context *service* (the upgraded
  socket is a request value, not a provided service).
- **Residual-2 parked (documented, no code)**: `@parcel/watcher` is a native `.node`
  addon, strictly opt-in (only `@effect/platform-node/NodeFileSystem/ParcelWatcher`;
  default `NodeFileSystem` uses `node:fs`, `NodeContext` never loads it). Native
  addons are an Option-C hard limit; opting in already fails cleanly (resolver CJS
  error). Real support would be Option-B/deno-node-services/FFI engine work.
- Full offline regression matrix green after the engine fix (effect/assert/node:vm/
  --transpile/trio/msgpackr/store-ws/p3-http/CJS-clean-error). The only remaining
  native gap is @parcel/watcher (see residual-2 above).

### P2-lite — resolver CJS classification + clean rejection + snapshot lint (landed)
- **Resolver** (`crates/inka-resolver`, no engine build): a `.js` file reached via the
  `import`/`node` `exports` condition is ESM by context (dual-package dist/esm pattern,
  e.g. find-my-way-ts/multipasta have no `"type":"module"`), so `exports_target` now
  returns an `EsmContext` (ByCondition vs Classify). `resolve_pkg_file` then rejects,
  with a stable CommonJS error, any served file that is `.cjs` or a non-`"module"`
  `.js` reached via `default`/plain-string/legacy `main`. Unpatched CJS roots/subpaths
  now fail cleanly instead of a cryptic "does not provide an export named 'default'".
  7 unit tests (CJS legacy main -> error, default-only-CJS -> error, type-module legacy
  + patched-ws shape -> file). Resolver tuple stays 1.0.0 (ABI unchanged).
- **Snapshot lint** (`inka pkg snapshot`): warns only when a *direct seed* package
  resolves CJS with no patch spec (top-level-only scan; transitive optional natives
  like msgpackr-extract/@parcel/watcher are the repo's patch-spec concern, not noise).
  Verified silent on the healthy store; fires on a seeded CJS package.
- Regressions green (effect/assert/node:vm/--transpile/trio/msgpackr/store-ws, offline).

### P1 outcome — patch layer landed (repo changes)
Implemented and verified end-to-end (scratch snapshot -> reseed -> offline runs):

- `crates/inka-patcher` — a **nested standalone workspace** (NOT a root member) that
  is the only consumer of the rolldown/oxc stack, because it pins tokio ^1.52 (via
  rolldown) while inka-runtime pins tokio =1.47.1: same workspace cannot hold both.
  Build it from inside its dir with the big-disk cargo home/target; `inka pkg
  snapshot` invokes it as a sibling binary (`$INKA_PATCHER` or next to the inka
  binary) so the fast default build never links rolldown. CLI:
  `inka-patcher apply --spec <patch.json> --node-modules <dir>`.
- Specs live under `patches/<pkg>/<version>/patch.json` (discovered by snapshot at
  `<dir of seed manifest>/patches`, or `--patches <dir>`), two kinds:
  - `bundle-esm` (entry/external/output/neutralizeEnv): rolldown bundle + post-pass.
    Shipped specs: `ws@8.21.3` (entry wrapper.mjs, natives external), `undici@7.29.1`
    (entry index.js), `mime@3.0.0` (entry index.js).
  - `file-patch` (`deleteFromMarker`): `msgpackr@1.12.1` truncates node-index.js at
    the `setExtractor` import, dropping the env-read/createRequire native block.
- Snapshot applies patches in the scratch node_modules BEFORE the tar and records
  them (`patched: [{name,version,kind}]`) in the seed-manifest record; seed/install
  carry the note through. Version guard: patcher refuses a spec whose package@version
  does not match the installed tree (npm resolved a different version -> fail loudly).
- Post-pass grew from the P0 version: hoists node builtin requires to default ESM
  imports in BOTH forms rolldown emits (`"crypto"` and `"node:assert"`), including
  subpaths (`fs/promises`, `util/types`); unknown lazies (`node:sqlite`) are left as
  catchable `__require` throws (never hoisted — an unused ESM import would hard-fail);
  the `__require` shim is now optional (bundles with no external requires, e.g. mime,
  need no shim). 6 hermetic unit tests in the crate.
- Verified offline against a freshly patched store: bare `effect`, `node:vm`, the
  multi-file `--transpile` zod/jsr-assert app, `@effect/platform` root + `MsgPack`
  (patched msgpackr round-trip, named + namespace imports), `trio.js` (effect +
  @effect/platform + @effect/platform-node NodeContext — pulls the ws/undici/mime
  leaves through their ESM import graph), and a bare `import "ws"` store app running a
  live 127.0.0.1 WebSocket echo (proves the exports rewrite + resolver import
  condition). No panics, no allow-env on the artifact.
- Remaining Option-C work: P2 (resolver CJS classification + engine clean rejection),
  P3 (full @effect/platform-node ladder: NodeRuntime.runMain HTTP/WebSocket smoke +
  regressions), P4 (README/plan.md docs + commits). P1 scratch: snaprel2/p1store under
  /tmp/opencode/inkam0.


`crates/inka-patcher` (new workspace member, the only rolldown dep): bundle a
spec'd package entry to ESM + apply the post-pass above + rewrite `package.json`
`exports` to the ESM build. Env-knob neutralization is per-package (spec-driven),
not generic. msgpackr stays a file-replacement (route `node.import` to pure-ESM
`index.js`) rather than a bundle. P0 artifacts/probe live under
`/tmp/opencode/inkam0/rdprobe` (+ `p0wsapp` = engine smoke).

---

### P0 verdict (evidence added during probe work) — §12 Option B
Probing Option B further showed the full cost. deno's require ops need not just a
NodeRequireLoader but the whole resolver stack in isolate state:
`NodeResolverRc`/`PackageJsonResolverRc` (node_resolver) + a CJS tracker
(package.json-"type"-aware) + node-modules-path semantics, assembled the way
`libdeno-0.3.2` does via `deno_resolver::factory` (WorkspaceFactory/ResolverFactory,
deno_json/config discovery, npm resolver, analysis caches). Hand-assembling it
directly against node_resolver is dozens of exact-API dependencies, each needing a
4-5 min engine compile to validate. Conclusion: Option B == porting Deno's CLI
resolver/npm stack (the phase-2 class of work); it is correct but a dedicated,
multi-session effort. Blueprint: adapt libdeno's worker_factory.rs /
deno_runtime_adapter.rs / node_loader.rs / deno_resolver_adapter.rs, substituting
our shared store for its npm/cache layers and keeping crates/inka-resolver as the
policy seam. P0 is therefore reclassified as "Option B feasibility study + port
design" rather than a quick probe. Keep the RealSys state-injection fix; do not
half-wire node services without the full resolver stack.

## 15. Per-project vendoring (W1–W3 landed; W4 = engine resolution milestone)

New store model (design in plan.md §"default store + name-keyed per-project
vendoring"): a machine default store (unchanged) PLUS per-project vendoring of
packages it doesn't cover. Vendored/ holds NAME-KEYED package roots (no
node_modules anywhere): vendored/ws/, vendored/@effect/platform/,
vendored/@jsr/std__assert/ (jsr = npm-mirror identity). One version per name is
enforced at add. Imports in app/vendored code resolve vendored -> default store
-> builtins; imports from inside store packages never consult vendored/.

- `inka add <pkg[@ver]>` (npm or jsr:@scope/name, exact x.y.z pins): dedupes when
  the default store already satisfies it (--force overrides); npm-installs the
  package alone, flattens the closure (conflict => error), auto-vendors any dep the
  store can't satisfy at its exact version, converts CJS leaves at add time via the
  shared inka-patcher + default patches (INKA_PATCHES / ./patches / <exe>/patches);
  hard CJS without a patch spec errors with guidance. Writes vendored.lock
  (roots+deps+conversions), syncs package.json dependencies AND deno.json imports
  (union; creates package.json only when neither file exists), ensures vendored/ is
  gitignored (dev posture).
- `inka remove <pkg>`: vendored only. Store-only package = no-op notice. Deletes the
  root + manifest lines, then pool-relative reverse-dep prunes orphaned vendored dep
  entries (flat set -> trivial). Deleting a forced override just falls back to the store.
- `inka vendor list|status|release|ignore`: list/coverage + git posture toggle
  (release removes the vendored/ ignore line for committed/offline builds).
- `inka build` embeds the vendored pool (whole-pool default; --vendor-closure is a
  future flag) into the artifact. The launcher AUTO-DETECTS a `vendored/` dir under
  the extracted root at run time (no manifest key needed).
- **W4 LANDED (resolution)**: resolver ABI v2 with two tiers chosen by the referrer —
  referrer under the default store root = store tier (today's semantics, builtins-first,
  never consults vendored); any other referrer (user/app/vendored code) = app/vendor
  tier: bare and npm:/jsr: pins resolve vendored/<name> (flat, jsr-mirror aware) ->
  store -> builtins last. Runtime reads INKA_VENDOR (launcher sets it when an artifact
  embeds vendored/), passes store+vendor to the resolver; expected ABI is 2. Layout/
  lock/manifests/git unchanged. Artifacts with vendored packages are PORTABLE: verified
  by copying the exe to a different directory and running offline. Store regression
  matrix (effect/trio/ws/http/p3) green after the change.
- Current CJS handling in `crates/inka` mirrors the resolver rule for detection
  (entry_is_commonjs). Scratch tests: /tmp/opencode/inkam0/vproj.

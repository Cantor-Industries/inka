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

# Porting inka to musl — plan (future task)

> **Status: proposed, not started.** This document records the investigation and
> the agreed design so the work can be picked up later without re-doing the
> research. Nothing here is implemented yet. It is intentionally detailed.
>
> References use the tree at 0.8.0 (runtime tuple `0.267.1`). Line numbers may
> drift; search by symbol if they no longer match.

## Goal

Produce a **musl** build of inka — the CLI (`inka`), the launcher
(`inka-launcher`), and the shared engine (`libinka_runtime-<tuple>.so`) — so
artifacts run on musl systems (Alpine and other minimal/container images)
without a glibc userland. Today only `x86_64-unknown-linux-gnu` is published and
the release is built on a glibc host, so a binary built on a newer distro fails
on older ones (`GLIBC_2.39 not found`). musl removes that class of problem.

## Decisions already taken

| Topic | Decision |
|---|---|
| Scope | Toolchain **and** engine (artifacts actually run on musl) |
| Linking | **Dynamic** musl (`-C target-feature=-crt-static`) — forced by `dlopen` |
| Packaging | Libc **subdirectory** (`runtime/musl/` vs `runtime/`), **same tuple** |
| Architectures | `x86_64-unknown-linux-musl` **and** `aarch64-unknown-linux-musl` |
| Build environment | `cargo-zigbuild` + Zig on the existing glibc runner (no Docker) |

## Why it is feasible — findings

### 1. Direct libc usage is trivial

- `crates/inka/src/main.rs:125` — the only direct `libc` call:
  `libc::signal(libc::SIGPIPE, libc::SIG_DFL)`.
- `crates/inka/src/Cargo.toml` depends on `libc = "0.2"`; the `libc` crate
  supports the musl targets unchanged.
- `crates/inka-runtime/Cargo.toml` and `crates/inka-bundler/Cargo.toml` pull
  `sys_traits` with the `libc` feature — also musl-portable.
- `crates/inka-launcher` has **no** `libc` dependency; it declares `atexit`
  itself (`crates/inka-launcher/src/main.rs:33`) and uses
  `libloading::os::unix` with `RTLD_GLOBAL`.
- `/proc` usage (launcher euid/liveness, `/dev/urandom`) exists on Alpine.

Nothing here blocks musl.

### 2. V8 already ships musl prebuilts — the usual blocker is gone

The engine is Deno-based: `deno_runtime 0.267.0` → `deno_core 0.412.0` →
`deno_v8 0.4.0` (a facade) → `v8` crate (rusty_v8) `150.4.0`.

- `deno_core` enables the `v8` backend with `{ simdutf }` and **no**
  `v8_enable_pointer_compression` / `v8_enable_sandbox`
  (`deno_core-0.412.0/Cargo.toml`: `[dependencies.v8] package = "deno_v8"`,
  `features = ["simdutf"]`; `deno_v8`'s `simdutf` feature forwards to
  `rusty_v8/simdutf`).
- Denoland's `rusty_v8` release `v150.4.0` publishes, among others:
  - `librusty_v8_release_x86_64-unknown-linux-musl.a.gz`
  - `librusty_v8_release_aarch64-unknown-linux-musl.a.gz`
  - `librusty_v8_simdutf_release_x86_64-unknown-linux-musl.a.gz`
  - `librusty_v8_simdutf_release_aarch64-unknown-linux-musl.a.gz`
  - (there is **no** `ptrcomp` musl archive, which is why the absence of
    pointer compression matters.)
- The `v8` crate's `build.rs` builds the prebuilt URL from
  `{features}_{profile}_{target}` (`static_lib_url`) and downloads it by
  default; musl from-source support (`use_musl=true`,
  `RUSTY_V8_MUSL_SYSROOT`, x86_64/aarch64 only) is the fallback if prebuilts
  are ever missing.

So `cargo build --target x86_64-unknown-linux-musl` should fetch the musl V8
archive automatically — **no `V8_FROM_SOURCE=1` needed**.

### 3. Other native dependencies are musl-friendly

From the resolved tree: `libffi`/`libffi-sys` (deno_ffi), `libuv-sys-lite`,
`libsqlite3-sys`/`rusqlite` (deno_node_sqlite, bundled SQLite), `zstd-sys`,
`brotli`, `zlib-rs`, `ring` and `aws-lc-sys` (cmake; used on Alpine),
`wgpu-core`/`wgpu-hal` (pure Rust, loads Vulkan at runtime — no link-time
graphics dep), and `deno_canvas` (pure Rust; no Skia). No `skia`/`skia-safe` in
the lock. `cmake` is already a documented build prerequisite. These are the
items to watch in the Phase 0 spike.

### 4. The one hard constraint: static musl cannot `dlopen`

Rust's `*-linux-musl` targets default to `crt-static = true` (fully static), and
**musl does not support `dlopen` from a static binary**. inka's architecture is
a tiny launcher that `dlopen`s the shared runtime:

- `crates/inka-launcher/src/main.rs` — `load_runtime_library` + symbol lookup.
- `crates/inka/src/run.rs` — `inka run` `dlopen`s the same runtime.

Therefore the launcher and the `inka` CLI must be **dynamically linked musl**
(`-C target-feature=-crt-static`), with musl's dynamic loader/libc available at
build time (Zig's musl provides it). A fully static single-file build would
require statically linking the ~100 MB engine into every artifact, defeating
inka's shared-engine design; that is explicitly out of scope.

Consequence: a musl toolchain produces **musl-only artifacts** (the embedded
launcher is musl, with a `/lib/ld-musl-*.so.1` interpreter). glibc and musl
artifacts cannot be unified.

## Work breakdown

### Phase 0 — spike (do this before changing the repo)

Install the toolchain on the runner/host:

```sh
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
# Zig + cargo-zigbuild (host, no Docker):
#   https://ziglang.org/download/  and  cargo install cargo-zigbuild
```

1. Build the engine for musl:
   ```sh
   cargo zigbuild --release --target x86_64-unknown-linux-musl -p inka-runtime
   ```
   Confirm the log shows the `librusty_v8_simdutf_release_x86_64-unknown-linux-musl.a.gz`
   download and that every native dep above compiles. Repeat for `aarch64`.
2. Build the CLI + launcher dynamic-musl:
   ```sh
   RUSTFLAGS="-C target-feature=-crt-static" \
     cargo zigbuild --release --target x86_64-unknown-linux-musl \
     -p inka --features bundle -p inka-launcher
   ```
   Inspect with `file`/`readelf -l`: expect a musl interpreter
   (`/lib/ld-musl-x86_64.so.1`) and `DT_NEEDED libc.so` (musl), **not** a static
   PIE.
3. Verify `dlopen` end to end: stage the musl `.so` in a runtime dir, build a
   hello artifact, and run it under a musl userland (x86_64: install the musl
   loader from Zig or the distro `musl` package; aarch64: `qemu-aarch64` plus a
   musl sysroot/loader).

**Exit criteria:** a hello artifact builds and runs musl-dynamic. If a native
dep or the Zig loader path fails, stop and reassess here before Phase 1.

Notes for the spike:
- Confirm the produced binary's `DT_NEEDED` soname matches Alpine's musl
  (`libc.so` / `/lib/ld-musl-*.so.1`); Zig may need explicit `--target` tuning.
- Running musl binaries on the glibc runner needs a musl loader (and `qemu` for
  aarch64). This is a test-environment requirement, separate from the build.

### Phase 1 — code changes

1. **Libc-scoped runtime discovery.**
   - `crates/inka-launcher/src/main.rs:488` `runtime_dirs()` and the filename
     parse at `:531`/`:534`.
   - `crates/inka/src/main.rs:25-26` (`FILENAME_PREFIX`/`FILENAME_SUFFIX`) and
     the scan at `:297-305`.
   - Design: glibc keeps the flat `runtime/`; musl builds additionally/instead
     scan `runtime/musl/`. Choose the subdir from the compile-time
     `cfg!(target_env = "musl")`. Filenames stay `libinka_runtime-<tuple>.so`,
     so the version parser and the launcher's `check_reported_version` are
     unchanged.
   - `INKA_RUNTIME_HOME` remains a flat, target-specific override (this is what
     `runtime-matrix.sh` stages), so tests keep working.
   - `inka doctor` should show the libc-appropriate dirs.
2. **`versions.json` + `inka update`.**
   - `.github/workflows/release.yml` produces the file; `crates/inka/src/update.rs`
     consumes it (`toolchain`/`runtime`/`runtime_sha256`).
   - Add a `targets` map keyed by triple, each with `{toolchain, runtime,
     runtime_sha256}`, and **keep the current top-level gnu fields** so older
     installers keep working. `update.rs` selects the entry by detected host
     target and installs the runtime into flat `runtime/` (gnu) or
     `runtime/musl/` (musl).
3. **`install.sh`** (`:192-197`).
   - Detect libc: glibc if `getconf GNU_LIBC_VERSION` succeeds (or
     `/lib/*/libc.so.6` exists); musl if `/lib/ld-musl-*.so.1` exists or
     `ldd --version 2>&1 | grep -qi musl`.
   - Map `uname -m` (`x86_64`, `aarch64`) + libc → target triple; install the
     runtime into the matching subdir; stop hardcoding
     `x86_64-unknown-linux-gnu`.
4. **Tuple**: unchanged (`0.267.1`). The engine behavior is identical; libc is a
   build flavor, not a new engine revision.

### Phase 2 — release / CI

- `.github/workflows/release.yml` (`TARGET` at `:41` and `:150`): keep the
  existing gnu stage; add a `zigbuild` stage building and staging, for each of
  `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`:
  - `inka-toolchain-<rel>-<target>.tar.gz` (CLI + launcher), dynamic-musl;
  - `libinka_runtime-<tuple>.so`;
  - `.sha256` sidecars; extend `versions.json` with the `targets` map.
- `scripts/ci/deno-pins.sh` and the V8 pin check should keep passing; V8 is
  fetched per target.
- New musl CI job: `cargo zigbuild` for both arches, then run
  `scripts/ci/runtime-matrix.sh` under a musl loader (x86_64) / `qemu-aarch64`
  (aarch64).
  - The native-addon case (`scripts/ci/runtime-matrix.sh:508`, currently
    `@parcel/watcher-linux-x64-glibc/watcher.node`) needs a **musl** `.node`
    prebuild; otherwise it must hit the strict-skip path
    (`INKA_MATRIX_ALLOW_SKIP`), which by design fails CI unless explicitly
    allowed — decide per environment.

### Phase 3 — docs / tests

- `docs/getting-started.md`, `docs/install-and-upgrade.md`,
  `docs/deployment.md`: musl prerequisites, libc detection, and the fact that a
  musl toolchain emits musl-only artifacts.
- Document that embedded N-API addons (`--external`) must be musl-built; glibc
  `.node` files will not load on musl (and vice versa).
- Unit tests: libc-scoped runtime discovery (launcher + CLI) and the
  `install.sh` target mapping (mirror the existing shell assertions).

## Risks / unknowns

- **Zig dynamic-musl interpreter/soname** must match Alpine's
  `/lib/ld-musl-*.so.1`. Verify `DT_NEEDED`/`PT_INTERP`; may need `--target`
  tuning or a sysroot. (Phase 0 gate.)
- **`aws-lc-sys` (cmake) and `wgpu`** are the most likely musl build friction.
- **aarch64** adds cross-compilation plus `qemu` to the test path.
- **Runner prerequisites**: Zig, `cargo-zigbuild`, `qemu-user`, and a musl
  loader for testing must be added to the self-hosted runner.
- **Test harness**: running musl artifacts on a glibc host (or aarch64 anywhere)
  requires a musl userland; the matrix may need a musl chroot/loader rather than
  the host shell.
- **Back-compat**: keep the top-level gnu fields in `versions.json` so an older
  `install.sh`/`inka update` still functions.

## Effort estimate

~1 session for the Phase 0 spike, then ~2–4 days of code/packaging, ~1–2 days of
release/CI/docs, all gated on Phase 0 passing. The engine build is the long pole
(V8 download + native C deps); most changes are mechanical libc/target plumbing.

## Reference index

- `crates/inka/src/main.rs:125` — `libc::signal(SIGPIPE)`.
- `crates/inka-launcher/src/main.rs:33,488,531,534` — `atexit`, `runtime_dirs`,
  runtime filename parsing.
- `crates/inka/src/main.rs:25-26,297-305` — runtime filename constants + scan.
- `crates/inka-runtime/Cargo.toml` — `deno_core`/`deno_runtime` pins and
  features; `crates/inka-runtime/build.rs` — snapshot build (`TARGET` aware).
- `crates/inka/src/update.rs` — toolchain/runtime asset selection.
- `install.sh:192-197` — hardcoded `Linux/x86_64 → gnu`.
- `.github/workflows/release.yml:41,150` — `TARGET`; asset staging + versions.json.
- `scripts/ci/runtime-matrix.sh:508` — glibc native-addon fixture.
- `docs/deployment.md`, `docs/install-and-upgrade.md` — distribution model.
- External: denoland/rusty_v8 `v150.4.0` release assets (musl archives);
  `cargo-zigbuild`; Zig musl libc/loader.

# inka

**Build one-file JavaScript/TypeScript executables that run on a shared, tuple-versioned Deno runtime.**

- **Tiny artifacts** — a bundled module + a ~380 KB launcher, not a ~100 MB engine in every binary.
- **Shared runtime** — the engine is installed once per user; inka executables load it at run time.
- **Offline builds** — `inka build` resolves and bundles imports (import maps, `npm:`, `jsr:`, `node_modules`) into one self-contained module.

The engine is built on [Deno](https://deno.com) — see the [Deno acknowledgment](#acknowledgments--deno).

## Install

Linux and WSL2 install with a single rootless command:

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/Cantor-Industries/inka/releases/latest/download/install.sh | sh
```

It installs the toolchain per-user (`~/.local/lib/inka`, shimmed at
`~/.local/bin/inka`) and provisions the shared engine (runtime) via
`inka update`. On Windows, install **WSL2** with Ubuntu and
run the same command inside it. macOS isn't published yet — **build from
source** (below).

Then confirm everything is ready:

```sh
inka doctor
```

`doctor` shows your installed runtimes plus project status (`package.json`/
`deno.json`/`deno.jsonc`, `node_modules`, `DENO_DIR`, bundling capability,
launcher). Pass an executable (`inka doctor ./app`) to inspect a built
artifact's module, runtime requirement, permissions, and payload instead. If
it's clean, you're ready.

> Newer runtime versions are fine — inka rolls forward to the newest installed
> tuple that satisfies each executable's manifest. Keep current with `inka
> update` (toolchain + runtime).
>
> **Beta builds:** `inka update --beta` (or `install.sh --beta`; on an older
> installer use `--version <tag>`) installs the newest prerelease. The
> installed toolchain's channel is the default, so a beta toolchain uses
> prerelease runtime tuples and stays on beta for `inka update`; `--beta`/
> `--stable` or `INKA_CHANNEL` override it. `--beta` is refused on a stable
> release — `inka update --beta` is the switcher. A stable release supersedes
> every beta of the same version.

### Building from source (macOS and other platforms)

**System prerequisites.** You need a Rust toolchain
([rustup](https://rustup.rs)) plus a C toolchain and development headers.

Debian / Ubuntu:

```sh
sudo apt-get update
sudo apt-get install -y build-essential libc6-dev pkg-config cmake \
  python3 perl curl git ca-certificates
# the runtime's C dependencies; add any the build reports as missing
sudo apt-get install -y libffi-dev zlib1g-dev liblzma-dev libuv1-dev
```

Fedora / RHEL:

```sh
sudo dnf groupinstall -y "Development Tools"
sudo dnf install -y glibc-devel pkgconf-pkg-config cmake python3 perl \
  git curl libffi-devel zlib-devel xz-devel libuv-devel
```

Arch:

```sh
sudo pacman -S --needed base-devel cmake python perl git curl
```

macOS: `xcode-select --install` (Xcode Command Line Tools).

`build-essential` pulls in `gcc`/`g++`/`make`/`libc6-dev`. The runtime links
`libuv` and `libffi` (with `zlib`/`xz` fallbacks); `bzip2` is not needed (a
pure-Rust backend is used). If a build error names another `-dev` package,
install that one.

> **glibc is forward-compatible, not backward.** A binary built on a newer
> distribution needs that glibc (or newer) at run time — a release built on
> Ubuntu 24.04 fails on 22.04 with `GLIBC_2.39 not found`. Build on the
> **oldest** distribution you need to support, or in a matching container.

The toolchain is a normal Rust workspace — clone the repo and build (bundling is
behind a feature, so enable it):

```sh
cargo build --release -p inka --features bundle
cargo build --release -p inka-launcher
```

The **runtime** is the heavy part (a Deno/V8 build, ~10–15 min on a roomy disk). Until release binaries exist for your platform, build it too and copy the `.so` into `~/.local/share/inka/runtime`:

```sh
CARGO_HOME=… CARGO_TARGET_DIR=… cargo build --release -p inka-runtime
mkdir -p ~/.local/share/inka/runtime
cp $CARGO_TARGET_DIR/release/libinka_runtime.so \
   ~/.local/share/inka/runtime/libinka_runtime-$(cat crates/inka-runtime/runtime-version).so
```

See [Building from source](docs/getting-started.md#5-building-from-source) for details.

## Get started

Write an app:

```ts
// app.ts
console.log(`hello ${Deno.args[0] ?? "world"} from Deno ${Deno.version.deno}`);
```

Bundle it into a single executable and run it:

```sh
inka build app.ts          # -> ./app   (manifest auto-generated, deny-by-default perms)
./app holla
```

While developing, skip the build entirely:

```sh
inka run app.ts holla
```

That's the whole loop: **write → `inka run` to iterate, `inka build` to ship**.

## What just happened

An inka executable is a **bundle of your entry and its dependencies** packed onto
a tiny launcher, plus a small manifest. When you run it, the launcher finds the
shared runtime you installed and dlopens it — the engine is never in your file,
so updating the machine's runtime updates every executable at once.

## Doing real things

Common tasks, one line each:

```sh
inka run -A app.ts                 # allow everything for a dev script
inka build -A app.ts               # bake allow-all into the executable
inka build --allow-env app.ts      # bake a single grant (least privilege)
inka build -P=server app.ts        # bake the `server` permission set from your config
inka build --external sharp app.ts # keep `sharp` unbundled but embed it from node_modules
inka build --minify app.ts         # minify the bundle
```

Permissions for an artifact come from explicit build-intent sources: the CLI
(`-A`/`--allow-*`, `-P=<set>`), `deno.json` `compile.permissions`, or an
`inka.permissions` marker in `deno.json`/`package.json` (string set name,
`"all"`, or an inline category map). With no source the artifact is
deny-by-default and `build` warns. See [Permissions](docs/permissions.md).

```ts
// dependencies come from your project's node_modules (or the Deno cache):
import { z } from "zod";
```

```sh
npm install zod           # (or pnpm/yarn/bun/deno install)
inka build app.ts         # zod is bundled into the executable
inka update               # fetch the newest toolchain + runtime for this machine
inka doctor               # machine + project state
```

- **Permissions** — inka executables are **deny-by-default**; you grant access explicitly at build or run time. Nothing is allowed until you say so.
- **Dependencies** — inka does no package management. It bundles from your project's `node_modules` (any package manager) and Deno's cache. See [Dependencies & resolution](docs/packages.md).
- **Native addons** — `.node` (N-API) packages must be left external (`--external <pkg>`) so their files are embedded; grant `ffi` (and usually `sys`).

## Learn more

- [Getting started](docs/getting-started.md)
- [`inka build`](docs/build.md)
- [`inka run`](docs/run.md)
- [Permissions](docs/permissions.md)
- [Dependencies & resolution](docs/packages.md)
- [Install & upgrade](docs/install-and-upgrade.md)
- [Troubleshooting](docs/troubleshooting.md)
- [CLI reference](docs/cli.md)
- [Architecture](docs/architecture.md) — how the launcher, tuples, and ABI work
- [Deployment](docs/deployment.md) — shipping inka and the executables it builds
- [Verifying a release](docs/verifying-a-release.md) — checklist for a published release

## License

MIT — see [`LICENSE`](LICENSE) (Copyright (c) 2026 Cantor Industries Authors).

## Acknowledgments — Deno

The inka engine is built on [Deno](https://deno.com), the work of the Deno
authors (Copyright (c) the Deno authors), distributed under the MIT license with
portions under Apache-2.0. See <https://github.com/denoland/deno>. Crates that
link Deno-project code (and the specific crates used) are acknowledged in their
own READMEs: `crates/inka-runtime` (`deno_core`, `deno_runtime`, `deno_error`,
`deno_semver`, `deno_graph`, `deno_cache_dir`) and `crates/inka-bundler`
(`deno_graph`, `deno_cache_dir`, `import_map`).

# inka

**Build one-file JavaScript/TypeScript executables that run on a shared, tuple-versioned Deno runtime.**

- **Tiny artifacts** — your file + a ~5 MB launcher, not a ~100 MB engine in every binary.
- **Shared runtime** — the engine is installed once per machine; inka executables load it at run time.
- **Offline packages** — import `npm:`/`jsr:` packages from a local store or vendor them into your project.

The engine is built on [Deno](https://deno.com) — see the [Deno acknowledgment](#acknowledgments--deno).

## Install

Linux and WSL2 install with a single rootless command:

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/Cantor-Industries/inka/releases/latest/download/install.sh | sh
```

It installs the toolchain per-user (`~/.local/lib/inka`, shimmed at
`~/.local/bin/inka`) and provisions the shared engine (runtime + resolver +
package store) via `inka update`. On Windows, install **WSL2** with Ubuntu and
run the same command inside it. macOS isn't published yet — **build from
source** (below).

Then confirm everything is ready:

```sh
inka doctor
```

`doctor` shows your installed runtimes, resolver, and package store, and tells
you if anything needs fixing. If it's clean, you're ready to go.

> Newer runtime versions are fine — inka rolls forward to the newest installed
> tuple that satisfies each executable's manifest. Keep current with `inka
> update` (toolchain + runtime + resolver + store).

### Building from source (macOS and other platforms)

The toolchain is a normal Rust workspace — clone the repo and build:

```sh
cargo build --release -p inka -p inka-launcher -p inka-resolver
```

The **runtime** is the heavy part (a Deno/V8 build, ~10–15 min on a roomy disk). Until release binaries exist for your platform, build it too and copy the `.so` into `~/.local/share/inka/runtime`:

```sh
CARGO_HOME=… CARGO_TARGET_DIR=… cargo build --release -p inka-runtime
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

Pack it into a single executable and run it:

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

An inka executable is just your source **concatenated onto a tiny launcher**, plus a small manifest. When you run it, the launcher finds the shared runtime you installed and dlopens it — nothing is bundled into your file, and updating the machine's runtime updates every executable at once.

## Doing real things

Common tasks, one line each:

```sh
inka run -A app.ts                 # allow everything for a dev script
inka build -P server app.ts        # bake the `server` permission set from your config
```

Permissions for an artifact come from explicit config sources (`-P <set>`, `deno.json` `compile.permissions`, an `inka.permissions` marker) — see [Permissions](docs/permissions.md).

```ts
// packages from the store work with no install step:
import { z } from "zod";
```

```sh
inka install              # vendor every dependency declared in package.json/deno.json
inka add nanoid           # vendor a single package the store doesn't provide
inka update               # fetch the newest runtime + resolver + store for this machine
inka doctor               # machine state: runtimes, resolver, store, vendored pool
```

- **Permissions** — inka executables are **deny-by-default**; you grant access explicitly at build or run time. Nothing is allowed until you say so.
- **Packages** — packages come from a machine-wide **store**, or you can **vendor** them into your project so the artifact is self-contained.

## Learn more

- [Getting started](docs/getting-started.md)
- [`inka build`](docs/build.md)
- [`inka run`](docs/run.md)
- [Permissions](docs/permissions.md)
- [Packages & the store](docs/packages.md)
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
`deno_semver`), `crates/inka` (`deno_ast`), and `crates/inka-resolver`
(`deno_semver`).

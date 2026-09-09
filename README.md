# inka

**Build one-file JavaScript/TypeScript executables that run on a shared, tuple-versioned Deno runtime.**

- **Tiny artifacts** — your file + a ~5 MB launcher, not a ~100 MB engine in every binary.
- **Shared runtime** — the engine is installed once per machine; inka executables load it at run time.
- **Offline packages** — import `npm:`/`jsr:` packages from a local store or vendor them into your project.

The engine is built on [Deno](https://deno.com) — see the [Deno acknowledgment](#acknowledgments--deno).

## Install

Pick the row for your machine, then install a runtime and check that everything is ready.

| You're on… | Do this |
|---|---|
| **Linux (Debian/Ubuntu)** | `sudo apt install ./inka_0.1.0_amd64.deb` — the easiest path |
| **Windows** | Install **WSL2** with Ubuntu, then run the same `.deb` command inside it (inka targets Linux and runs in WSL transparently) |
| **macOS / other OS** | **Build from source** — release binaries for those platforms aren't shipped yet (see below) |

After the toolchain, install a runtime tuple (this is the shared engine inka executables load):

```sh
inka install 0.266.0 --from <release-base>
```

> Newer runtime versions are fine — inka rolls forward to the newest installed tuple that satisfies each executable's manifest.

Now confirm everything is ready:

```sh
inka doctor
```

`doctor` shows your installed runtimes, resolver, and package store, and tells you if anything needs fixing. If it's clean, you're ready to go.

### Building from source (macOS and other platforms)

The toolchain is a normal Rust workspace — clone the repo and build:

```sh
cargo build --release -p inka -p inka-launcher -p inka-resolver
```

The **runtime** is the heavy part (a Deno/V8 build, ~10–15 min on a roomy disk). Until release binaries exist for your platform, build it too and copy the `.so` into `~/.inka-runtime`:

```sh
CARGO_HOME=… CARGO_TARGET_DIR=… cargo build --release -p inka-runtime
cp $CARGO_TARGET_DIR/release/libinka_runtime.so ~/.inka-runtime/libinka_runtime-0.266.0.so
```

See [Building from source](docs/install-and-upgrade.md#building-from-source) for details.

## Get started

Write an app:

```ts
// app.ts
console.log(`hello ${Deno.args[0] ?? "world"} from Deno ${Deno.version.deno}`);
```

Pack it into a single executable and run it:

```sh
inka build app.ts          # -> ./app   (manifest auto-generated, deny-by-default perms)
./app kook
```

While developing, skip the build entirely:

```sh
inka run app.ts kook
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
inka add nanoid          # vendor a package the store doesn't provide (project-local)
inka doctor              # machine state: runtimes, resolver, store, vendored pool
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

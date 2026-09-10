# Getting started

This guide gets you from zero to a working inka executable. It assumes nothing
about your setup beyond a supported machine.

## Requirements

- **A 64-bit Linux** (Debian/Ubuntu is easiest), **or** Windows with **WSL2**
  (Ubuntu), **or** a machine you're willing to build from source on (macOS and
  other OSes aren't shipped as binaries yet).
- A Rust toolchain **only if** you build from source (see below).

## 1. Install the toolchain

Pick the row that matches your machine.

| You're on… | Install |
|---|---|
| **Linux (Debian/Ubuntu)** | `sudo apt install ./inka_0.1.0_amd64.deb` |
| **Windows** | Install [WSL2](https://learn.microsoft.com/windows/wsl/install) with Ubuntu, open the Ubuntu terminal, then run the same `.deb` command inside it |
| **macOS / other OS** | Build from source (see [Building from source](#5-building-from-source)) |

The `.deb` puts `inka` on your `PATH` (it installs to `/usr/lib/inka` and
symlinks `/usr/bin/inka`). It is **toolchain-only** — the shared engine is
installed next, per user.

## 2. Install a runtime

The runtime is the shared engine your executables load at run time. The `.deb`
may already bundle it (plus the resolver and a seeded package store); to install
or update to the newest, run:

```sh
inka update
```

> Releases publish runtime tuples (with checksums) at a release base URL.
> `inka update` resolves the newest automatically; `inka update <version>
> --from <base>` installs a specific tuple, and newer versions are fine — inka
> rolls forward automatically.

## 3. Check that everything is ready

```sh
inka doctor
```

`inka doctor` prints your runtime dir, installed runtimes + resolver, and your
package store, and warns about anything missing or mismatched. If it shows a
runtime and says no warnings, you're ready.

## 4. Build your first app

Write a program:

```ts
// app.ts
console.log(`hello ${Deno.args[0] ?? "world"} from Deno ${Deno.version.deno}`);
```

Pack it and run it:

```sh
inka build app.ts      # -> ./app  (manifest auto-generated, deny-by-default perms)
./app kook
```

While developing, skip the build and run the file directly through the runtime:

```sh
inka run app.ts kook
```

That's the core loop: **`inka run` to iterate, `inka build` to ship.**

## 5. Building from source

Only needed for platforms without a release, or to hack on inka itself.

The toolchain is a normal Rust workspace:

```sh
git clone https://github.com/Cantor-Industries/inka
cd inka
cargo build --release -p inka -p inka-launcher -p inka-resolver
# target/release/inka
```

The **runtime** (`crates/inka-runtime`) is the heavy part — a Deno/V8 build that
takes ~10–15 minutes and wants a roomy disk:

```sh
CARGO_HOME=… CARGO_TARGET_DIR=… cargo build --release -p inka-runtime
cp $CARGO_TARGET_DIR/release/libinka_runtime.so ~/.local/share/inka/runtime/libinka_runtime-0.266.0.so
```

Keep `inka-launcher` next to the `inka` binary, or set `INKA_LAUNCHER`. See
[Install & upgrade](install-and-upgrade.md) for the full runtime story, and the
individual crate READMEs under `crates/` for build/test commands.

## Next steps

- [`inka build`](build.md) — manifests, permissions, TypeScript, multi-file apps
- [`inka run`](run.md) — the dev runner and its permission flags
- [Permissions](permissions.md) — why things are deny-by-default and how to grant
- [Packages & the store](packages.md) — using and vendoring `npm:`/`jsr:` packages
- [Troubleshooting](troubleshooting.md) — `doctor`, exit codes, common errors
- [CLI reference](cli.md) — every command at a glance
- [Architecture](architecture.md) — how the launcher, tuples, and ABI work

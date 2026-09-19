# Getting started

This guide gets you from zero to a working inka executable. It assumes nothing
about your setup beyond a supported machine.

## Requirements

- **A 64-bit Linux** (`x86_64`), **or** Windows with **WSL2** (Ubuntu), **or** a
  machine you're willing to build from source on (macOS and other OSes aren't
  shipped as binaries yet).
- `curl` (used by the installer and `inka update` over HTTP).
- A Rust toolchain **only if** you build from source (see below).

## 1. Install inka

Linux and WSL2 install rootlessly with one command:

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/Cantor-Industries/inka/releases/latest/download/install.sh | sh
```

It installs the toolchain under `~/.local/lib/inka` (shimmed at
`~/.local/bin/inka`), adds `~/.local/bin` to your `PATH`, and provisions the
shared engine (runtime). On macOS/other OSes, build
from source (see [Building from source](#5-building-from-source)).

> `install.sh --help` lists options (`--version`, `--prefix`, `--no-engine`,
> `--uninstall`, …). `--from <dir-or-url>` installs from a mirror or local
> staging dir.

## 2. Install / update the engine

The installer already provisioned the shared engine (runtime).
To update everything to the newest release:

```sh
inka update
```

> Releases publish runtime tuples (with checksums) at a release base URL.
> `inka update` resolves the newest automatically; `inka update <version>
> --from <base>` installs a specific tuple, and newer versions are fine — inka
> rolls forward automatically. A runtime tuple is only fetched when it is
> missing or newer; older tuples are left in place.

## 3. Check that everything is ready

```sh
inka doctor
```

`inka doctor` prints your runtime dir, installed runtimes, and project status
(config, `node_modules`, `DENO_DIR`, bundling, launcher), and warns about
anything missing. If it shows a runtime and no warnings, you're ready.

## 4. Build your first app

Write a program:

```ts
// app.ts
console.log(`hello ${Deno.args[0] ?? "world"} from Deno ${Deno.version.deno}`);
```

Pack it and run it:

```sh
inka build app.ts      # -> ./app  (manifest auto-generated, deny-by-default perms)
./app holla
```

While developing, skip the build and run the file directly through the runtime:

```sh
inka run app.ts holla
```

That's the core loop: **`inka run` to iterate, `inka build` to ship.**

## 5. Building from source

Only needed for platforms without a release, or to hack on inka itself.

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

The toolchain is a normal Rust workspace:

```sh
git clone https://github.com/Cantor-Industries/inka
cd inka
cargo build --release -p inka --features bundle
cargo build --release -p inka-launcher
# target/release/inka
```

The **runtime** (`crates/inka-runtime`) is the heavy part — a Deno/V8 build that
takes ~10–15 minutes and wants a roomy disk. Build it with `--features desktop`
so `inka desktop` apps can share it, and build the desktop shim alongside:

```sh
CARGO_HOME=… CARGO_TARGET_DIR=… cargo build --release -p inka-runtime --features desktop
cargo build --release -p inka-desktop-shim
mkdir -p ~/.local/share/inka/runtime
cp $CARGO_TARGET_DIR/release/libinka_runtime.so \
   ~/.local/share/inka/runtime/libinka_runtime-$(cat crates/inka-runtime/runtime-version).so
```

Keep `inka-launcher` next to the `inka` binary, or set `INKA_LAUNCHER`. See
[Install & upgrade](install-and-upgrade.md) for the full runtime story, and the
individual crate READMEs under `crates/` for build/test commands.

## Next steps

- [`inka build`](build.md) — manifests, permissions, TypeScript, multi-file apps
- [`inka run`](run.md) — the dev runner and its permission flags
- [Desktop apps](desktop.md) — `inka desktop`, the shared-runtime native shell
- [Permissions](permissions.md) — why things are deny-by-default and how to grant
- [Dependencies & resolution](packages.md) — bundling `npm:`/`jsr:` packages
- [Troubleshooting](troubleshooting.md) — `doctor`, exit codes, common errors
- [CLI reference](cli.md) — every command at a glance
- [Architecture](architecture.md) — how the launcher, tuples, and ABI work

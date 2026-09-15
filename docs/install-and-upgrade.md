# Install & upgrade

## Install (Linux / WSL2)

inka is distributed as release artifacts fetched by a bootstrap script — no
package manager, no root. It installs per-user:

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/Cantor-Industries/inka/releases/latest/download/install.sh | sh
```

The script:

1. downloads the **toolchain** archive (CLI + launcher) for
   `x86_64-unknown-linux-gnu`, verifies its `sha256`, and installs it under
   `<prefix>/lib/inka` (default prefix `$HOME/.local`);
2. symlinks `<prefix>/bin/inka` and adds `<prefix>/bin` to your `PATH`
   (`--no-modify-path` to skip);
3. runs `inka update` to provision the shared **runtime** under
   `~/.local/share/inka`.

### Options

The defaults are sensible: install to `~/.local` (toolchain `<prefix>/lib/inka`,
shim `<prefix>/bin/inka`), add `~/.local/bin` to `PATH`, and provision the
runtime under `~/.local/share/inka`.

To pass options through the pipe, use `sh -s --` (the `--` stops `sh` from
parsing them):

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/Cantor-Industries/inka/releases/latest/download/install.sh \
  | sh -s -- --prefix "$HOME/.local"
```

| Option | Effect |
|---|---|
| `--version <tag>` | pin the release (assets fetched from that tag) |
| `--from <dir-or-url>` | release base override (mirror / local staging) |
| `--prefix <dir>` | toolchain prefix (default `$HOME/.local`) |
| `-y`, `--yes` | non-interactive (accepted for compatibility) |
| `--no-modify-path` | do not edit shell rc files |
| `--no-engine` | skip the runtime |
| `--no-runtime` | skip the runtime `.so` |
| `--force` | reinstall the toolchain even if current |
| `--uninstall` | remove the toolchain (and engine) |

Environment overrides: `INKA_REPO` (default `Cantor-Industries/inka`),
`INKA_PREFIX` (default `$HOME/.local`), and `INKA_RELEASE_BASE`.

To pin **both** the script and the assets, fetch the script from the tag rather
than `latest`:

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/Cantor-Industries/inka/releases/download/v0.4.0/install.sh | sh
```

`install.sh --help` lists everything.

> Only Linux/`x86_64` is published today. On Windows, install **WSL2** with
> Ubuntu and run the same command inside it. macOS is not published yet — build
> from source (below).

## Verify

```sh
inka doctor
```

`doctor` prints the runtime dirs, the installed runtimes, project status
(config, `node_modules`, `DENO_DIR`, bundling, launcher), and any warnings. If it
shows a runtime and no warnings, you're ready.

## First app

```ts
// app.ts
console.log(`hello ${Deno.args[0] ?? "world"} from Deno ${Deno.version.deno}`);
```

```sh
inka run app.ts kook      # iterate
inka build app.ts         # -> ./app  (manifest derived from config, deny-by-default)
./app kook
```

## Upgrade

```sh
inka update               # toolchain + runtime, newest
inka update <ver> --from <base>   # a specific runtime tuple (offline/pinned)
```

> **Coming from 0.4.x?** 0.5.0 is a clean break: the package store and vendoring
> were removed and `inka build` now bundles. Just re-run `install.sh` once — it
> detects a pre-0.5.0 install and resets the old toolchain and runtime before
> provisioning the new release. `inka update` alone will not cross this boundary.
> From 0.5.0 on, `inka update` self-updates normally.

`inka update` reconciles every component against `<base>/versions.json`:

- **toolchain** — self-updates when a newer release exists (only for
  installer-managed installs, i.e. those with a `VERSION` marker);
- **runtime** — installs only when missing or a newer tuple exists; never
  downgrades, and leaves older tuples in place.

The base defaults to the GitHub latest-release URL and is overridable with
`--from`, `INKA_RELEASE_BASE`, or `INKA_RT_SOURCE`.

## Where state lives

| State | Path | Override |
|---|---|---|
| Toolchain (`inka`, launcher) | `<prefix>/lib/inka` (`$HOME/.local/lib/inka`) | `--prefix` at install |
| Toolchain shim | `<prefix>/bin/inka` | — |
| Runtime | `~/.local/share/inka/runtime` | `INKA_RUNTIME_HOME` |
| Deno cache (read for `jsr:`) | `~/.cache/deno` | `DENO_DIR` |

`~/.local/share` is `$XDG_DATA_HOME` when set.

## Building from source

Building needs a Rust toolchain and a C toolchain plus development headers (and,
for the runtime, its C dependencies). See
[Building from source](getting-started.md#5-building-from-source) for the
per-distribution package list — and build on the oldest glibc you must support.

```sh
git clone https://github.com/Cantor-Industries/inka
cd inka
cargo build --release -p inka --features bundle
cargo build --release -p inka-launcher
```

The **runtime** (`crates/inka-runtime`) is the heavy part — a Deno/V8 build that
takes ~10–15 minutes and wants a roomy disk:

```sh
CARGO_HOME=… CARGO_TARGET_DIR=… cargo build --release -p inka-runtime
mkdir -p ~/.local/share/inka/runtime
cp $CARGO_TARGET_DIR/release/libinka_runtime.so \
   ~/.local/share/inka/runtime/libinka_runtime-$(cat crates/inka-runtime/runtime-version).so
```

Keep `inka-launcher` next to the `inka` binary, or set `INKA_LAUNCHER`.

## Uninstall

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/Cantor-Industries/inka/releases/latest/download/install.sh \
  | sh -s -- --uninstall
```

This removes the toolchain, the `PATH` block, and `~/.local/share/inka`
(runtime).

## Versioning

Two independent version lines:

- **Toolchain** — the release tag (`v0.4.0`); the `inka` crate version tracks it.
- **Runtime tuple** — the `deno_runtime` base (`0.xxx.0`) plus an inka runtime
  revision: `0.266.0` → `0.266.1`, …, `0.266.6`; when the base moves to
  `0.267.0`, revisions restart at `0.267.1`. See
  `crates/inka-runtime/runtime-version`.

Artifacts roll forward to the newest installed runtime tuple that satisfies
their manifest, so a runtime upgrade never requires rebuilding artifacts.

# Install & upgrade

## Install (Linux / WSL2)

inka is distributed as release artifacts fetched by a bootstrap script — no
package manager, no root. It installs per-user:

```sh
curl --proto '=https' --tlsv1.2 -sSf \
  https://raw.githubusercontent.com/Cantor-Industries/inka/master/install.sh | sh
```

The script:

1. downloads the **toolchain** archive (CLI + launcher + patcher + curated
   patches) for `x86_64-unknown-linux-gnu`, verifies its `sha256`, and installs
   it under `<prefix>/lib/inka` (default prefix `$HOME/.local`);
2. symlinks `<prefix>/bin/inka` and adds `<prefix>/bin` to your `PATH`
   (`--no-modify-path` to skip);
3. runs `inka update` to provision the shared **runtime** and **resolver** under
   `~/.local/share/inka`, then seeds the default package **store** as its own
   step.

Useful options: `--version <tag>` (pin a release), `--from <dir-or-url>`
(mirror/local staging), `--prefix <dir>`, `--no-engine` (skip runtime +
resolver; the store still seeds), `--no-runtime`/`--no-resolver`/`--no-store`,
`--uninstall`. Run `install.sh --help` for the full list.

> Only Linux/`x86_64` is published today. On Windows, install **WSL2** with
> Ubuntu and run the same command inside it. macOS is not published yet — build
> from source (below).

## Verify

```sh
inka doctor
```

`doctor` prints the runtime dirs, the installed runtime/resolver (with ABI), the
default store (packages + seed `sha256`), and any warnings. If it shows a
runtime and no warnings, you're ready.

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
inka update               # toolchain + runtime + resolver + store, newest
inka update <ver> --from <base>   # a specific runtime tuple (offline/pinned)
```

`inka update` reconciles every component against `<base>/versions.json`:

- **toolchain** — self-updates when a newer release exists (only for
  installer-managed installs, i.e. those with a `VERSION` marker);
- **runtime** — installs only when missing or a newer tuple exists; never
  downgrades, and leaves older tuples in place;
- **resolver** — same policy as the runtime;
- **store** — replaces `node_modules` when the release's snapshot `sha256`
  differs.

The base defaults to the GitHub latest-release URL and is overridable with
`--from`, `INKA_RELEASE_BASE`, or `INKA_RT_SOURCE`.

## Where state lives

| State | Path | Override |
|---|---|---|
| Toolchain (`inka`, launcher, patcher, patches) | `<prefix>/lib/inka` (`$HOME/.local/lib/inka`) | `--prefix` at install |
| Toolchain shim | `<prefix>/bin/inka` | — |
| Runtime/resolver | `~/.local/share/inka/runtime` | `INKA_RUNTIME_HOME` |
| Default package store | `~/.local/share/inka/store` | `INKA_STORE` |

`~/.local/share` is `$XDG_DATA_HOME` when set. Runtime selection also falls back
to `/usr/local/lib/inka-runtime`.

## Building from source

```sh
git clone https://github.com/Cantor-Industries/inka
cd inka
cargo build --release -p inka -p inka-launcher -p inka-resolver
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
curl --proto '=https' --tlsv1.2 -sSf \
  https://raw.githubusercontent.com/Cantor-Industries/inka/master/install.sh \
  | sh -s -- --uninstall
```

This removes the toolchain, the `PATH` block, and `~/.local/share/inka`
(runtime + store).

## Versioning

Three independent version lines:

- **Toolchain** — the release tag (`v0.2.0`); the `inka` crate version tracks it.
- **Runtime tuple** — the `deno_runtime` base (`0.xxx.0`) plus an inka runtime
  revision: `0.266.0` → `0.266.1`, `0.266.2`, …; when the base moves to
  `0.267.0`, revisions restart at `0.267.1`. See
  `crates/inka-runtime/runtime-version`.
- **Resolver** — its own crate version (`crates/inka-resolver/Cargo.toml`), e.g.
  `1.0.1`.

Artifacts roll forward to the newest installed runtime tuple that satisfies
their manifest, so a runtime upgrade never requires rebuilding artifacts.

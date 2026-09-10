# Install & upgrade

## Toolchain (`.deb`)

```sh
sudo apt install ./inka_0.1.0_amd64.deb
```

Installs `inka`, `inka-launcher`, `inka-patcher`, and curated `patches/` under
`/usr/lib/inka`, with `/usr/bin/inka` symlinked there (all binaries resolve by
exe-adjacency).

The package **may bundle the shared engine and a store snapshot**. Its
`postinst` (best-effort — never fails `dpkg`):

1. copies any bundled `libinka_runtime-<v>.so` / `libinka_resolver-<v>.so` into
   `/usr/local/lib/inka-runtime`;
2. seeds the installing account's store (`$SUDO_USER`-aware) from the bundled
   snapshot;
3. if no runtime is present and none was bundled, runs `inka update`
   (network) and otherwise prints guidance.

`apt remove inka` leaves per-user `~/.local/share/inka` and the system-wide
`/usr/local/lib/inka-runtime` copies intact.

## Where state lives

| State | Path | Override |
|---|---|---|
| System runtime/resolver | `/usr/local/lib/inka-runtime` | — |
| Per-user runtime/resolver | `~/.local/share/inka/runtime` | `INKA_RUNTIME_HOME` |
| Default package store | `~/.local/share/inka/store` | `INKA_STORE` |

`~/.local/share` is `$XDG_DATA_HOME` when set.

## Updating the engine

```sh
inka update        # newest runtime + resolver + store from the channel
inka update <ver> --from <base>   # a specific tuple (offline/pinned)
```

`inka update` reads `<base>/versions.json`, installs only the runtime/resolver
that are behind, and syncs the store when its recorded `sha256` differs. It
never downgrades and never re-downloads an unchanged runtime. The base defaults
to the GitHub latest-release URL and is overridable with `--from`,
`INKA_RELEASE_BASE`, or `INKA_RT_SOURCE`.

Run `inka doctor` afterward.

## Building from source

Toolchain:

```sh
git clone https://github.com/Cantor-Industries/inka
cd inka
cargo build --release -p inka -p inka-launcher -p inka-resolver
```

Runtime (the heavy part — a Deno/V8 build, ~10–15 min on a roomy disk):

```sh
CARGO_HOME=… CARGO_TARGET_DIR=… cargo build --release -p inka-runtime
mkdir -p ~/.local/share/inka/runtime
cp $CARGO_TARGET_DIR/release/libinka_runtime.so \
   ~/.local/share/inka/runtime/libinka_runtime-0.266.0.so
```

Keep `inka-launcher` next to the `inka` binary, or set `INKA_LAUNCHER`.

## Versioning

Toolchain versions use Debian-sortable ordering (pre-releases as `0.1.0~rc1`).
Runtime tuple filenames carry the `deno_runtime` version; artifacts roll forward
to the newest installed tuple that satisfies their manifest, so a runtime
upgrade never requires rebuilding artifacts.

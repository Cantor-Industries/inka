# Deployment

How inka itself is distributed and how the executables it builds are deployed.
inka's core property drives every choice here: **the engine is not in the file.**
A built artifact is a small launcher + your payload + a manifest; it loads a
shared per-machine Deno runtime tuple (`libinka_runtime-<v>.so`) plus an optional
package store. That makes runtime upgrades independent of your artifacts.

## What ships where

| Piece | Where it lives | How it updates |
|---|---|---|
| `inka`, `inka-launcher`, `inka-patcher`, `patches/` (toolchain) | `.deb` → `/usr/lib/inka` (+ `/usr/bin/inka` symlink), or a manually colocated prefix | dpkg/apt (toolchain `.deb`) |
| `libinka_runtime-<v>.so`, `libinka_resolver-<v>.so` | per-user `~/.inka-runtime` (`INKA_RUNTIME_HOME`) | `inka install <ver> --from <base>` |
| default store (`node_modules` + record) | `~/.inka-runtime/store` (`INKA_STORE`) | `inka pkg seed` / shipped with `inka install` store payload |

The toolchain `.deb` deliberately does **not** bundle a runtime or store, so
`apt upgrade` of inka never touches per-user engine state, and per-machine
engine/store installs stay isolated per user.

## `.deb` package (toolchain)

### Layout & discovery

```
/usr/lib/inka/inka
/usr/lib/inka/inka-launcher
/usr/lib/inka/inka-patcher
/usr/lib/inka/patches/{ws,undici,mime,msgpackr}/<ver>/patch.json
/usr/bin/inka -> ../lib/inka/inka
/usr/share/doc/inka/{README.md,copyright}
```

All binaries resolve by **exe-adjacency**, so the symlinked `/usr/bin/inka`
(which resolves to `/usr/lib/inka/inka`) finds the launcher, `inka-patcher`, and
`patches/` next to itself. No environment variables are required.

### Build

```sh
# release build (inka, inka-launcher, inka-resolver) + patcher + patches
scripts/build-deb.sh

# with overrides (see the script for all envs)
INKA_DEB_VERSION=0.2.0 \
DEB_MAINTAINER="Acme <admin@acme.example>" \
DEB_HOMEPAGE="https://acme.example/inka" \
INKA_PATCHER=/path/to/inka-patcher \
scripts/build-deb.sh
```

Output: `target/inka_<ver>_amd64.deb`. Install with
`sudo apt install ./target/inka_<ver>_amd64.deb` (or `dpkg -i`). No runtime is
installed by the package — each user then runs:

```sh
inka install <version> --from <release-base-url>   # runtime + resolver
inka pkg seed   --from <release-base-url>           # default store (flat GitHub release assets)
```

> GitHub Release assets are flat files, so the store payload (`store.tar.gz` +
> `seed-manifest.json`) is seeded with `inka pkg seed --from <base>`. A
> **directory/static-host** release may instead lay the store under a `store/`
> subdir (`store/seed-manifest.json`, `store/store.tar.gz`), in which case
> `inka install` seeds it automatically.
inka doctor                                        # health check
```

### Uninstall / upgrade semantics

- Removing the `.deb` (`apt remove inka`) keeps `~/.inka-runtime` intact.
- Reinstalling a newer `.deb` overwrites in place (dpkg file ownership +
  `md5sums`); `patches/` is additive, so new curated spec versions can be added
  without touching anything else.
- No `/etc` state is written, so there are no conffiles to migrate.

## Upgrades

The design splits upgrades into two independent axes:

1. **Toolchain upgrades** — new `.deb`, same stable `/usr/lib/inka` paths. Do
   this whenever you want new CLI/build/vendor features. Artifacts never need a
   rebuild because of a toolchain change.
2. **Engine / store upgrades** — per user, out of band of apt:
   - install a newer tuple: `inka install <newver> --from <base>`;
   - reseed on store drift: `inka pkg seed` (or the release's store payload);
   - `inka doctor` reports installed runtime/resolver versions and warns when a
     `vendored.lock` was built against a different default store.

Version ordering: use Debian-sortable versions so apt upgrades resolve
correctly — release tags directly; pre-releases as e.g. `0.1.0~rc1` (the `~`
sorts below `0.1.0`). `scripts/build-deb.sh` takes `INKA_DEB_VERSION` or the
newest git tag.

### Future: an apt repository

The package metadata (`Section`, `Architecture`, `Maintainer`, `Homepage`) is
set so the same `.deb` can later be published in an apt repo
(`Packages.gz` + signed `Release`) for `apt install inka` / `apt upgrade`
without package rework. Not scaffolded yet.

## App deployment

A built artifact is self-contained **except** for dependencies that resolve from
the default store at run time (anything not vendored). Two deployment styles:

- **Fully portable single exe**: build on a machine/step without a store
  (`INKA_STORE` pointing at an empty dir) so `inka add` vendors the whole
  closure; copy the exe anywhere. Recommended for third-party distribution.
- **Store-mode app**: build with the target store present and deploy the exe +
  ensure the machine's store identity matches (compare `inka doctor` /
  `vendored.lock` store note); reseed if it drifted. Best when one store serves
  many apps on a host/container.

### Containers

Use a base image that installs the toolchain `.deb` and then seeds the runtime +
store once, so every app in the image runs against the shared tuple:

```dockerfile
FROM debian:bookworm-slim
COPY inka_0.1.0_amd64.deb /tmp/inka.deb
RUN apt-get update && apt-get install -y /tmp/inka.deb && rm /tmp/inka.deb \
 && /usr/lib/inka/inka install 0.266.0 --from <release-base-url>
# copy your built executable(s) in and run them
```

App builds can happen in a separate builder stage (with the store present for
deterministic store-mode resolution) and only the resulting exe copied in.

## Operations

- After any install/upgrade/deploy, run `inka doctor`; it reports runtime dir,
  installed runtimes/resolvers + ABI, store packages + sha, and the vendored
  pool, and prints warnings (missing resolver, ABI mismatch, store drift).
- Releases carry `.sha256` sidecars and are verified on install. Today that
  verifies integrity, not authenticity — sign checksums (e.g. minisign) and pin
  a trust anchor for production distribution.

## Release CI (tags → GitHub Release assets)

`.github/workflows/release.yml` runs on a self-hosted runner whenever a `v*`
tag is pushed. It builds and publishes everything in one pass:

1. **Build** the toolchain (`inka`, `inka-launcher`, `inka-resolver`), the
   `inka-patcher`, and the runtime `.so` (always — the runtime tuple filename is
   the `deno_runtime` pin, e.g. `0.266.0`, so a runtime bump is just a bump of
   that pin in `crates/inka-runtime/Cargo.toml`).
2. **Snapshot** the default store (`inka pkg snapshot`) with the built patcher.
3. **Stage + package**: toolchain binaries, curated `patches/`, the runtime +
   resolver `.so`, `store.tar.gz` + `seed-manifest.json`, the `.deb`, and
   `.sha256` sidecars + `versions.json`.
4. **Smoke** the staged release in a throwaway `INKA_RUNTIME_HOME`/`INKA_STORE`
   (doctor, store-mode `effect`/`hono`/`ws`, an artifact build+run, and a
   vendored auto-conversion). Any failure aborts before publishing.
5. **Publish** the assets to the GitHub Release for the tag (automatic; refuses
   to re-publish a tag that already has a release).

Runner setup: register a self-hosted runner (label `self-hosted`) and give its
environment `CARGO_HOME` and `CARGO_TARGET_DIR` pointing at a roomy disk (the
Deno runtime build needs several GB and ~10–15 minutes). Tag releases as
`v<ver>` and protect the tag with a rule requiring signatures.

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
| `libinka_runtime-<v>.so`, `libinka_resolver-<v>.so` | system `/usr/local/lib/inka-runtime`, or per-user `~/.local/share/inka/runtime` (`INKA_RUNTIME_HOME`) | `inka update` (bundled in the `.deb` when it changed; postinst installs it system-wide) |
| default store (`node_modules` + record) | per-user `~/.local/share/inka/store` (`INKA_STORE`) | `inka update` (seeded from the `.deb` payload on install) |

The `.deb` bundles the runtime and store **only when they changed** since the
previous release, keeping toolchain-only releases small. Its `postinst`
(best-effort, never fails dpkg) copies any bundled runtime/resolver into
`/usr/local/lib/inka-runtime`, seeds the installing account's XDG store from the
bundled snapshot, and otherwise runs `inka update`. Per-user engine state is
never touched by `apt upgrade`.

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
`sudo apt install ./target/inka_<ver>_amd64.deb` (or `dpkg -i`). The package
bundles the runtime/resolver (and a store snapshot) when they changed since the
previous release; its `postinst` installs them system-wide and seeds the
installing user's store. If the package carries no runtime, the `postinst`
runs `inka update` best-effort. To fetch or refresh later:

```sh
inka update        # newest runtime + resolver + store from the release channel
inka update <ver> --from <release-base-url>   # a specific tuple
inka doctor        # health check
```

> GitHub Release assets are flat files; `inka update` reads `versions.json` and
> `seed-manifest.json` directly from the base. A **directory/static-host**
> release works the same way.

### Uninstall / upgrade semantics

- Removing the `.deb` (`apt remove inka`) keeps per-user `~/.local/share/inka`
  and the system-wide `/usr/local/lib/inka-runtime` engine copies intact.
- Reinstalling a newer `.deb` overwrites in place (dpkg file ownership +
  `md5sums`); `patches/` is additive, so new curated spec versions can be added
  without touching anything else.
- No `/etc` state is written, so there are no conffiles to migrate.

## Upgrades

The design splits upgrades into two independent axes:

1. **Toolchain upgrades** — new `.deb`, same stable `/usr/lib/inka` paths. Do
   this whenever you want new CLI/build/vendor features. Artifacts never need a
   rebuild because of a toolchain change.
2. **Engine / store upgrades** — system-wide or per user, out of band of apt:
   - `inka update` fetches the newest runtime + resolver and syncs the store;
   - `inka update <ver> --from <base>` installs a specific tuple;
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

Use a base image that installs the toolchain `.deb`; its `postinst` installs the
bundled runtime/store (or runs `inka update`), so every app in the image runs
against the shared tuple:

```dockerfile
FROM debian:bookworm-slim
COPY inka_0.1.0_amd64.deb /tmp/inka.deb
RUN apt-get update && apt-get install -y /tmp/inka.deb && rm /tmp/inka.deb
# the postinst provisions the runtime/store; refresh explicitly if needed:
# RUN /usr/lib/inka/inka update
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
2. **Snapshot** the default store (`inka internal snapshot-store`) with the built patcher.
3. **Stage + package**: toolchain binaries, curated `patches/`, the runtime +
   resolver `.so`, `store.tar.gz` + `seed-manifest.json`, the `.deb` (bundling
   the runtime/store only when they changed vs the previous release), and
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

After a release is published, run the checklist in
[Verifying a published release](verifying-a-release.md) (integrity checks,
clean install, store seed, store-mode + vendored smokes).

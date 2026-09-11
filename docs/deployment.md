# Deployment

How inka itself is distributed and how the executables it builds are deployed.
inka's core property drives every choice here: **the engine is not in the file.**
A built artifact is a small launcher + your payload + a manifest; it loads a
shared per-machine runtime tuple (`libinka_runtime-<v>.so`) plus an optional
package store. That makes runtime upgrades independent of your artifacts.

## Distribution model

inka ships as GitHub Release assets fetched by a bootstrap script
(`install.sh`), not as an OS package. Installation is per-user and rootless:

- `install.sh` downloads the toolchain archive, verifies its `sha256`, installs
  it under `<prefix>/lib/inka` (default `~/.local`), symlinks
  `<prefix>/bin/inka`, and then runs `inka update` to provision the runtime,
  resolver, and store.
- `inka update` keeps the toolchain, runtime, resolver, and store current from
  the same release channel. The runtime and resolver are fetched only when
  missing or newer; older tuples are retained for roll-forward.

## What ships where

| Piece | Where it lives | How it updates |
|---|---|---|
| Toolchain (`inka`, `inka-launcher`) | `<prefix>/lib/inka`, shimmed at `<prefix>/bin/inka` | `install.sh`, then `inka update` |
| `libinka_runtime-<v>.so`, `libinka_resolver-<v>.so` | `~/.local/share/inka/runtime` (`INKA_RUNTIME_HOME`) | `inka update` |
| default store (`node_modules` + record) | `~/.local/share/inka/store` (`INKA_STORE`) | `inka update` (sha-gated) |

## Release assets

Each `v*` tag publishes:

- `inka-toolchain-<rel>-x86_64-unknown-linux-gnu.tar.gz` (+ `.sha256`) — CLI +
  launcher;
- `libinka_runtime-<runtime>.so` (+ `.sha256`) — the shared runtime tuple;
- `libinka_resolver-<resolver>.so` (+ `.sha256`) — the resolution engine;
- `store.tar.gz` (+ `.sha256`) + `seed-manifest.json` — the default store
  snapshot;
- `install.sh` and `versions.json`.

`versions.json` records the release, the toolchain block (version/target/archive/
sha256), the runtime tuple and its base `deno_runtime`, the resolver, and the
runtime `sha256`. `inka doctor` prints the installed identities.

## Release CI (tags → GitHub Release assets)

`.github/workflows/release.yml` runs on a self-hosted runner whenever a `v*`
tag is pushed:

1. **Build** the toolchain (`inka`, `inka-launcher`, `inka-resolver`) and the
   runtime `.so`. The runtime tuple version comes from
   `crates/inka-runtime/runtime-version` (base `deno_runtime` + inka revision);
   the resolver version from its crate.
2. **Snapshot** the default store (`inka internal snapshot-store`).
3. **Stage + package**: the toolchain tarball, engine assets, `store.tar.gz` +
   `seed-manifest.json`, `install.sh`, `versions.json`, and `.sha256` sidecars.
4. **Smoke** the staged release in a throwaway prefix/store by running
   `install.sh --from <stage>` and then doctor, store-mode imports, an artifact
   build+run, permission enforcement, and a vendored CJS require. Any failure
   aborts before publishing.
5. **Publish** the assets to the GitHub Release for the tag (refuses to
   re-publish a tag that already has a release).

Runner setup: register a self-hosted runner (label `self-hosted`) and give its
environment `CARGO_HOME` and `CARGO_TARGET_DIR` pointing at a roomy disk (the
Deno runtime build needs several GB and ~10–15 minutes). Tag releases as
`v<ver>` and protect the tag with a rule requiring signatures.

After a release is published, run the checklist in
[Verifying a published release](verifying-a-release.md).

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

Use a base image and run the installer (its per-user layout also works under
`root`, installing to `/root/.local`):

```dockerfile
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y curl ca-certificates \
 && curl --proto '=https' --tlsv1.2 -fsSL \
      https://github.com/Cantor-Industries/inka/releases/latest/download/install.sh | sh \
 && rm -rf /var/lib/apt/lists/*
ENV PATH=/root/.local/bin:$PATH
# copy your built executable(s) in and run them
```

App builds can happen in a separate builder stage (with the store present for
deterministic store-mode resolution) and only the resulting exe copied in.

## Operations

- After any install/upgrade/deploy, run `inka doctor`; it reports runtime dirs,
  installed runtimes/resolvers + ABI, store packages + sha, and the vendored
  pool, and prints warnings.
- Releases carry `.sha256` sidecars and are verified on install. Today that
  verifies integrity, not authenticity — sign checksums (e.g. minisign) and pin
  a trust anchor for production distribution.

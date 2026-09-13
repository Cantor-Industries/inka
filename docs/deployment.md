# Deployment

How inka itself is distributed and how the executables it builds are deployed.
inka's core property drives every choice here: **the engine is not in the file.**
A built artifact is a small launcher + a bundled module + a manifest; it loads a
shared per-machine runtime tuple (`libinka_runtime-<v>.so`). That makes runtime
upgrades independent of your artifacts.

## Distribution model

inka ships as GitHub Release assets fetched by a bootstrap script
(`install.sh`), not as an OS package. Installation is per-user and rootless:

- `install.sh` downloads the toolchain archive, verifies its `sha256`, installs
  it under `<prefix>/lib/inka` (default `~/.local`), symlinks
  `<prefix>/bin/inka`, and then runs `inka update` to provision the runtime.
- `inka update` keeps the toolchain and runtime current from the same release
  channel. The runtime is fetched only when missing or newer; older tuples are
  retained for roll-forward.

## What ships where

| Piece | Where it lives | How it updates |
|---|---|---|
| Toolchain (`inka`, `inka-launcher`) | `<prefix>/lib/inka`, shimmed at `<prefix>/bin/inka` | `install.sh`, then `inka update` |
| `libinka_runtime-<v>.so` | `~/.local/share/inka/runtime` (`INKA_RUNTIME_HOME`) | `inka update` |

There is no package store: dependencies come from each project's own
`node_modules` and the Deno cache (`DENO_DIR`) at build/run time.

## Release assets

Each `v*` tag publishes:

- `inka-toolchain-<rel>-x86_64-unknown-linux-gnu.tar.gz` (+ `.sha256`) — CLI +
  launcher;
- `libinka_runtime-<runtime>.so` (+ `.sha256`) — the shared runtime tuple;
- `install.sh` and `versions.json`.

`versions.json` records the release, the toolchain block (version/target/archive/
sha256), the runtime tuple and its base `deno_runtime`, and the runtime `sha256`.
`inka doctor` prints the installed identities.

## Release CI (tags → GitHub Release assets)

`.github/workflows/release.yml` runs on a self-hosted runner whenever a `v*`
tag is pushed:

1. **Build** the toolchain (`inka --features bundle`, `inka-launcher`) and the
   runtime `.so`. The tuple version comes from
   `crates/inka-runtime/runtime-version`.
2. **Stage + package**: the toolchain tarball, runtime `.so`, `install.sh`,
   `versions.json`, and `.sha256` sidecars.
3. **Smoke** the staged release in a throwaway prefix by running
   `install.sh --from <stage>` and then doctor, a simple run + build, permission
   enforcement, an `--external` artifact, an import-map→jsr artifact (offline at
   run time), and the runtime matrix. Any failure aborts before publishing.
4. **Publish** the assets to the GitHub Release for the tag (refuses to
   re-publish a tag that already has a release).

Runner setup: register a self-hosted runner (label `self-hosted`) and give its
environment `CARGO_HOME` and `CARGO_TARGET_DIR` pointing at a roomy disk (the
Deno runtime build needs several GB and ~10–15 minutes). Tag releases as
`v<ver>` and protect the tag with a rule requiring signatures.

After a release is published, run the checklist in
[Verifying a published release](verifying-a-release.md).

## App deployment

A built artifact is **self-contained**: `inka build` bundles the reachable
dependency graph, and `--external <pkg>` packages are embedded from
`node_modules`. Copy the executable anywhere; it needs only a compatible shared
runtime on the target machine (installed by `install.sh`/`inka update`).

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

App builds can happen in a separate builder stage (with the project's
`node_modules` and, for `jsr:`, a populated `DENO_DIR`); only the resulting
executable needs to be copied in.

## Operations

- After any install/upgrade/deploy, run `inka doctor`; it reports runtime dirs,
  installed runtimes, and project status (config, `node_modules`, `DENO_DIR`,
  bundling capability, launcher), and prints warnings.
- Releases carry `.sha256` sidecars and are verified on install. Today that
  verifies integrity, not authenticity — sign checksums (e.g. minisign) and pin
  a trust anchor for production distribution.

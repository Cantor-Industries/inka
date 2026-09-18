# Release process

Maintainer-facing notes for cutting inka releases. For the user-facing install
and upgrade story see [Install & upgrade](install-and-upgrade.md).

## Version sources

| Thing | Source of truth |
|---|---|
| Toolchain release | the `v*` tag; `crates/inka/Cargo.toml` `version` must equal its base |
| Runtime tuple | `crates/inka-runtime/runtime-version` |
| deno_runtime pin | `crates/inka-runtime/Cargo.toml` (`=X.Y.Z`) |
| Baked identity | `INKA_BUILD_VERSION` / `INKA_BUILD_COMMIT` (release build only) |

Two counters are **independent**: the toolchain release counter
(`0.8.1-beta.N`) advances once per toolchain beta, while the runtime tuple
counter (`X.Y.Z-beta.N`) advances only when the engine changes. A toolchain beta
with no engine change ships the existing runtime tuple.

`rc` prereleases map to the **beta** channel (`-beta.`/`-rc.` ⇒ beta).

## Tag format

- Stable: `v<major>.<minor>.<patch>` (e.g. `v0.8.1`).
- Beta: `v<base>-beta.<n>-<short-hash>` (e.g. `v0.8.1-beta.2-f97fa59`).
  `version.sh` enforces the shape and that `crates/inka/Cargo.toml` matches the
  base.

## Cutting a beta

1. Bump `crates/inka/Cargo.toml` (and sibling crates) to the target base; if the
   engine changed, bump `crates/inka-runtime/runtime-version` (and the tuple
   suffix).
2. Pick the next toolchain counter:
   ```sh
   scripts/ci/next-beta.sh 0.8.1        # -> 0.8.1-beta.3
   scripts/ci/next-beta.sh --runtime    # -> next runtime tuple beta
   ```
3. Tag and push the tag (`v0.8.1-beta.3-<short-hash>`); the `release` workflow
   builds, smoke-tests, and publishes a GitHub **prerelease**.
4. The workflow bakes `INKA_BUILD_VERSION=$REL` and
   `INKA_BUILD_COMMIT=${GITHUB_SHA:0:7}` into `inka`/`inka-launcher`, so the
   shipped binary reports the exact release and defaults to the beta channel.

## Behavior invariants

- The installed toolchain's channel is the default for `run`/`build`/`doctor`/
  `update`; `--beta`/`--stable` (then `INKA_CHANNEL`) override it.
- `--beta` on `build`/`run`/`doctor` is refused on a stable release with the
  ``run `inka update --beta` to switch to the beta channel`` hint; `update
  --beta` is allowed (the switcher). Unknown `INKA_CHANNEL` is a hard error.
- `build` records `channel=beta` when building for beta or when the resolved
  `runtime`/`tested-against` spec is a prerelease. The launcher admits
  prereleases when `channel=beta` or any version slot is a prerelease (plus the
  `INKA_CHANNEL=beta` artifact-side override).
- Discovery (`inka update --beta`, `install.sh --beta`) selects the
  **max-by-version** prerelease, never trusting API order, and skips candidates
  whose assets are missing.

## Install doc caveat

A **stable** installer older than 0.8.1 has no `--beta` flag. Until a stable
with `--beta` ships, install betas with an exact tag
(`install.sh --version <tag>`) or the beta release's own installer. After that,
`releases/latest/download/install.sh --beta` works. See
[Install & upgrade](install-and-upgrade.md#beta-releases).

## Verify

```sh
cargo fmt --all -- --check
cargo clippy --locked -p inka --bin inka -p inka-launcher -p inka-format -- -D warnings
cargo clippy --locked -p inka --features bundle --all-targets -- -D warnings
cargo test --locked -p inka --bin inka
cargo test --locked -p inka --bin inka --features bundle
cargo test --locked -p inka-launcher
cargo test --locked -p inka-format
scripts/ci/deno-pins.sh
bash -n install.sh scripts/ci/*.sh scripts/spike.sh
```

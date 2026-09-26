# Release process

Maintainer-facing notes for cutting inka releases. For the user-facing install
and upgrade story see [Install & upgrade](install-and-upgrade.md).

## Version sources

| Thing | Source of truth |
|---|---|
| Toolchain release | the `v*` tag; `crates/inka/Cargo.toml` `version` must equal its base |
| Runtime tuple | `crates/inka-runtime/runtime-version` |
| deno_runtime pin | `crates/inka-runtime/Cargo.toml` (`=X.Y.Z`) |
| laufey pin | `crates/inka/src/desktop.rs` (`LAUFEY_VERSION`, `LAUFEY_SUMS`) and `laufey = "=X.Y.Z"` |
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

## Release notes

Two templates live at the repo root and are substituted by the release workflow
(`{{REL}}`, `{{RUNTIME}}`, `{{TAG}}`, `{{CHANNEL}}`); everything through the
closing `-->` in the leading comment is stripped before publishing.

- `RELEASE_NOTES_BETA.md` — used for a `-beta.`/`-rc.` tag.
- `RELEASE_NOTES.md` — used for a stable tag.

**Notes are incremental: describe only what changed in *this* release.** Do not
restate features from earlier releases (beta notes = the delta since the
previous beta; stable notes = the delta since the previous stable). Repeat an
older item only when it is genuinely needed for context, and prefer a link to
the earlier release. Update the relevant file as part of "Cutting a beta"/tagging
so the tagged commit carries the notes.

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

## Desktop

- The release workflow builds the shared runtime with `-p inka-runtime --features
  desktop` on **both** targets (Linux and `windows-latest`; the latter needs
  `LIBCLANG_PATH`) and the per-app shim with `-p inka-desktop-shim`; both the
  runtime library and the shim (staged into the toolchain archive) are required
  for `inka desktop` to work. The Windows runtime `.dll` is desktop-enabled.
- The workflow also mirrors the pinned laufey archive(s) as release assets
  (`scripts/ci/laufey-asset.sh <backend> <target>` reads the archive name and
  SHA-256 from `desktop.rs`): `laufey-cef-*` on Linux, `laufey-webview-*` on
  Windows. They are recorded in `versions.json` under `targets[<target>].laufey`.
  `inka desktop --installer` provisions the shared engine and CEF runtime from
  these assets, so app installers never reach laufey's host.
- The toolchain is built with `INKA_BUILD_TAG=$GITHUB_REF_NAME` baked in, so a
  generated installer pins the exact release (not `releases/latest`); override
  at install time with `--engine-base`/`INKA_RELEASE_BASE`.
- `scripts/ci/smoke.sh` builds a CEF app with `--installer`, runs the generated
  installer against the staging dir into a throwaway HOME/XDG, and asserts the
  engine + CEF provisioning, the app tree, and (under `xvfb-run`) launch.
- `crates/inka-runtime/runtime-version` therefore tracks a **desktop-enabled**
  tuple: bumping it (e.g. to force `inka update` to fetch an engine with the
  laufey ABI) is the tool for changing the desktop engine without changing the
  Deno base.
- laufey is pinned at `0.7.0` (`LAUFEY_API_VERSION == 34`). The backend archives
  are checksum-pinned in `LAUFEY_SUMS`. Bumping laufey means updating the
  `LAUFEY_VERSION`/`LAUFEY_SUMS` constants, the `laufey = "=X.Y.Z"` pins, and the
  API-version assert in the runtime when the ABI changes.
- `crates/inka-runtime/src/desktop_js.rs` vendors Deno 2.9.7's desktop JS
  (byte-for-byte except the `serde_json` path); re-sync it when `deno_runtime` or
  laufey is bumped.
- Attribution: laufey is MIT (Copyright (c) Divy Srivastava); the desktop JS and
  `cli/rt_desktop` adaptation are MIT (Copyright (c) the Deno authors). Keep
  these notices in the docs/READMEs.

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
cargo clippy --locked -p inka-runtime --features desktop --all-targets -- -D warnings
cargo clippy --locked -p inka-desktop-shim --all-targets -- -D warnings
cargo test --locked -p inka --bin inka
cargo test --locked -p inka --bin inka --features bundle
cargo test --locked -p inka-launcher
cargo test --locked -p inka-desktop-shim
cargo test --locked -p inka-format
scripts/ci/deno-pins.sh
scripts/ci/desktop-js-pins.sh
bash -n install.sh scripts/ci/*.sh scripts/spike.sh
```

<!--
  Maintainer notes. Everything through this closing `-->` is stripped from the
  published body by the release workflow (`.github/workflows/release.yml`).

  Beta release body template. Used when the tag carries a `-beta.<n>` suffix;
  the workflow substitutes `{{REL}}` (e.g. `0.8.1-beta.1`), `{{RUNTIME}}`
  (`crates/inka-runtime/runtime-version`), `{{TAG}}` (the exact git tag) and
  `{{CHANNEL}}` before publishing. Do not hardcode versions.
-->
# inka {{REL}} (beta) — runtime tuple {{RUNTIME}}

> **This is a pre-release.** It is published as a GitHub prerelease and is
> **not** installed by the default channel. Use it only if you want to test the
> next release early.

## Install this beta

```sh
# the exact tag (works with any installer, including the current stable one)
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/Cantor-Industries/inka/releases/latest/download/install.sh \
  | sh -s -- --version {{TAG}}

# this release's own installer (0.8.1+; resolves the newest beta)
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/Cantor-Industries/inka/releases/download/{{TAG}}/install.sh \
  | sh -s -- --beta
```

Already installed? `inka update --beta` moves the toolchain and runtime to the
newest beta; `inka update --stable` (or re-running the stable `install.sh`)
returns to the stable channel.

## How the beta channel works

- Beta runtime tuples are named `<version>-beta.<n>` and the toolchain version
  is `{{REL}}`. The eventual stable release of the same base supersedes every
  beta, so graduation is automatic.
- **The installed toolchain's channel is the default.** A beta toolchain selects
  prerelease runtime tuples for `run`/`build`/`doctor` and plain `inka update`
  stays on beta (no downgrade). A stable toolchain never selects a beta runtime.
- Override per invocation with `--beta`/`--stable` or `INKA_CHANNEL=beta`/
  `stable` (an unknown `INKA_CHANNEL` value is a hard error). `--beta` and a
  prerelease `--runtime` are **refused on a stable release**; `inka update
  --beta` is the channel switcher.
- `inka build` records `channel=beta` in the artifact when building for beta
  (explicitly, via the toolchain channel, or via a prerelease runtime spec), so
  the launcher may pick a prerelease tuple.
- Beta assets carry `.sha256` sidecars and are checksum-verified on install.

## What's in this beta

- **Fixed: import-map npm/jsr subpath resolution.** `inka run` now resolves a
  subpath import such as `import x from "@scope/pkg/sub"` when `@scope/pkg` is
  mapped to `npm:`/`jsr:` in `deno.json`. Deno's import map expands that entry
  to the URL form `npm:/@scope/pkg@ver/sub`; the runtime now parses it with
  `deno_semver` (the same parser `inka build` uses) instead of failing with
  `invalid package name ''`.
- **Beta release channel.** `inka update --beta` / `install.sh --beta` install
  the newest beta. The installed toolchain's channel now drives defaults, so a
  beta toolchain selects prerelease runtime tuples without extra flags.
- **Accurate version output.** `inka --version`, every `ui::title`, and
  `inka-launcher --version` now print the installed release (e.g.
  `{{REL}}`), not the crate version; `inka doctor` shows
  `toolchain {{REL}} (<short-hash>)` and the effective `channel`.
- **Stable releases refuse `--beta`.** On a stable release, `--beta` (and
  `INKA_CHANNEL=beta`) is a hard error with a hint to run `inka update --beta`.
  Dev builds remain beta-capable with a stable default.

## Assets

- `inka-toolchain-{{REL}}-x86_64-unknown-linux-gnu.tar.gz` — CLI + launcher
- `libinka_runtime-{{RUNTIME}}.so` — shared runtime tuple
- `install.sh` + `versions.json` — bootstrap installer + version record

`.sha256` sidecars are published for the toolchain and runtime.

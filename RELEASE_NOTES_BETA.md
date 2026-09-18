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
# newest beta (toolchain + runtime)
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/Cantor-Industries/inka/releases/latest/download/install.sh | sh -s -- --beta

# this exact beta, {{TAG}}
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/Cantor-Industries/inka/releases/latest/download/install.sh \
  | sh -s -- --version {{TAG}}
```

Already installed? `inka update --beta` moves the toolchain and runtime to the
newest beta; `inka update` returns to the stable channel.

## How the beta channel works

- Beta runtime tuples are named `<version>-beta.<n>` and the toolchain version
  is `{{REL}}`. The eventual stable release of the same base supersedes every
  beta, so graduation is automatic.
- Prerelease runtime tuples are **excluded from normal selection**. Use
  `inka build --beta` (records `channel=beta` in the artifact), `inka run
  --beta`, or `INKA_CHANNEL=beta` to opt in.
- Beta assets carry `.sha256` sidecars and are checksum-verified on install.

## What's in this beta

- **Fixed: import-map npm/jsr subpath resolution.** `inka run` now resolves a
  subpath import such as `import x from "@scope/pkg/sub"` when `@scope/pkg` is
  mapped to `npm:`/`jsr:` in `deno.json`. Deno's import map expands that entry
  to the URL form `npm:/@scope/pkg@ver/sub`; the runtime now parses it with
  `deno_semver` (the same parser `inka build` uses) instead of failing with
  `invalid package name ''`.
- **Beta release channel.** `inka update --beta` / `install.sh --beta` install
  the newest beta; `inka` selects prerelease runtime tuples only when asked.

## Assets

- `inka-toolchain-{{REL}}-x86_64-unknown-linux-gnu.tar.gz` — CLI + launcher
- `libinka_runtime-{{RUNTIME}}.so` — shared runtime tuple
- `install.sh` + `versions.json` — bootstrap installer + version record

`.sha256` sidecars are published for the toolchain and runtime.

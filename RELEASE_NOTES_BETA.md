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

### `inka desktop` — native desktop apps on the shared runtime

Build a native desktop app that reuses the machine's one inka engine instead of
embedding a ~150 MB copy per app. A prebuilt laufey window (system
WebKitGTK on Linux) loads a small per-app shim, which unpacks your bundled app
and loads the shared, desktop-enabled runtime. Linux/webview today.

- **Package an app** (`inka desktop <entry>`) whose entry serves HTTP
  (`export default { fetch }` or `Deno.serve`); the shell runs it on a loopback
  port and navigates exactly one window to it. Bundling matches `inka build`
  (import maps, `npm:`/`jsr:`, `node_modules`), and `--payload <dir>` packs an
  already-built directory.
- **Framework projects** (`inka desktop .`): detection vendored from Deno —
  **Vite, Astro, Fresh, Remix, React Router, SvelteKit, Nuxt, SolidStart,
  TanStack Start**. The project's build task runs, the framework entrypoint is
  generated and bundled, and its output is shipped. Next.js is detected only to
  give an actionable error (its server can't be bundled into a single-file
  payload).
- **`deno.json` `desktop` config** with CLI overrides: `app.name`,
  `app.identifier` (reverse-DNS `.desktop` id), `app.icons.linux`, `backend`,
  `output.linux`, `release.baseUrl`, `errorReporting.url`, and top-level
  `version`. Config is discovered by walking up from the current directory;
  malformed fields warn, and an invalid identifier/backend is a hard error.
- **Dev workflow** (runs the source tree directly, no packaging):
  - `--hmr` watches and hot-replaces changed modules via the inspector,
    reloading the window when a change can't be applied in place.
  - `--inspect[=host:port]` / `--inspect-brk` / `--inspect-wait` start a CDP
    multiplexer for the runtime isolate (attach with `chrome://inspect`; use the
    `/deno` target on the webview backend).
- **Auto-update & error reporting**: `Deno.autoUpdate` with a signed
  `latest.json` (SHA-256-verified patches, optional ed25519 signature) and a
  staged `.update`/`.backup`/`.update-ok` swap; uncaught JS errors **and Rust
  panics** POST to `errorReporting.url` over the runtime's own HTTP client.
- **Packaging output**: an app directory (`<App>`, `<App>.so`, `runtime-version`,
  `<id>.desktop`, optional `AppIcon.png`) plus a `<App>.tar.gz`.

The desktop-enabled engine is runtime tuple `{{RUNTIME}}`; the release runtime
is built with `--features desktop`, and `inka doctor` now reports the selected
tuple's advertised capabilities (including `desktop`) and the shim.

### Also in the 0.8.1 beta line

- **Fixed: import-map npm/jsr subpath resolution.** `inka run` resolves a
  subpath import such as `import x from "@scope/pkg/sub"` when `@scope/pkg` is
  mapped to `npm:`/`jsr:` in `deno.json`, instead of failing with
  `invalid package name ''`.
- **Beta release channel.** `inka update --beta` / `install.sh --beta` install
  the newest beta; the installed toolchain's channel drives defaults, so a beta
  toolchain selects prerelease runtime tuples without extra flags.
- **Accurate version output.** `inka --version`, every `ui::title`, and
  `inka-launcher --version` print the installed release (e.g. `{{REL}}`);
  `inka doctor` shows `toolchain {{REL}} (<short-hash>)` and the effective
  `channel`.
- **Stable releases refuse `--beta`.** On a stable release, `--beta` (and
  `INKA_CHANNEL=beta`) is a hard error with a hint to run `inka update --beta`.
  Dev builds remain beta-capable with a stable default.

## Assets

- `inka-toolchain-{{REL}}-x86_64-unknown-linux-gnu.tar.gz` — CLI + launcher +
  desktop shim (`libinka_desktop_shim.so`)
- `libinka_runtime-{{RUNTIME}}.so` — shared runtime tuple (desktop-enabled)
- `install.sh` + `versions.json` — bootstrap installer + version record

`.sha256` sidecars are published for the toolchain and runtime.

<!--
  Maintainer notes. Everything through this closing `-->` is stripped from the
  published body by the release workflow (`.github/workflows/release.yml`), so
  it is safe to keep internal guidance here.

  The body uses `{{REL}}` (tag minus `v`, e.g. `0.8.0`) and `{{RUNTIME}}`
  (`crates/inka-runtime/runtime-version`, e.g. `0.267.1`) placeholders; the
  workflow substitutes them when staging the release. Do not hardcode versions.
-->
# inka {{REL}} — runtime tuple {{RUNTIME}}

An engine maintenance release. The runtime is rebased on **Deno 2.9.8**
(`deno_runtime 0.268.0`) and the tuple moves to `{{RUNTIME}}`. There are no new
commands and no changes to the inka surface; artifacts keep working and roll
forward to the new tuple.

The bump carries Deno's security updates: the compromised eMudhra certificate
authorities are no longer trusted, and the read permission check for dynamically
imported local files in compiled binaries is tightened (inka's loader already
confined those loads to the execution tree or an explicit read grant).

## Upgrade

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/Cantor-Industries/inka/releases/latest/download/install.sh | sh
```

Then keep the toolchain and runtime current with `inka update`.

## Breaking changes

- **New runtime tuple `{{RUNTIME}}` (Deno 2.9.8).** `inka update` installs it;
  existing artifacts keep working and select the newest installed tuple that
  satisfies their manifest. The previous tuple remains a rollback path.

## Under the hood

- **Engine rebased on Deno 2.9.8**: `deno_runtime 0.268.0`, `deno_core 0.413.0`,
  `deno_cache_dir 0.52.0`, `deno_config 0.110.0`, `deno_graph 0.111.1`,
  `deno_resolver 0.91.0`, `node_resolver 0.98.0`. V8 `150.4.0` and TypeScript
  `6.0.3` are unchanged.
- **Source-neutral seam.** The tag-to-tag audit found only version bumps in
  `libs/node_resolver`, `libs/resolver`, `libs/config`, and `libs/cache_dir`;
  the `deno_core` changes are internal and the vendored desktop/HMR sources are
  byte-identical. The runtime was rechecked with the deno-pin and desktop-JS
  gates, the ABI symbol gate, the unit tests, clippy, and the full runtime
  contract matrix.

## Assets

- `inka-toolchain-{{REL}}-x86_64-unknown-linux-gnu.tar.gz` — CLI + launcher +
  desktop shim (`libinka_desktop_shim.so`)
- `libinka_runtime-{{RUNTIME}}.so` — shared runtime tuple (desktop-enabled)
- `inka-toolchain-{{REL}}-x86_64-pc-windows-msvc.zip` — Windows CLI + launcher +
  desktop shim (`libinka_desktop_shim.dll`)
- `libinka_runtime-{{RUNTIME}}.dll` — Windows shared runtime (desktop-enabled)
- `laufey-cef-*.tar.gz` (Linux) / `laufey-cef-*.zip` (Windows) and
  `laufey-webview-*.zip` (Windows) — pinned backend mirrors
- `install.sh` / `install.ps1` + `versions.json` — bootstrap installers + version
  record

`.sha256` sidecars are published for the toolchains and runtimes.

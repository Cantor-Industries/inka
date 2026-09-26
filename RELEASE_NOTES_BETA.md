<!--
  Maintainer notes. Everything through this closing `-->` is stripped from the
  published body by the release workflow (`.github/workflows/release.yml`).

  Beta release body template. Used when the tag carries a `-beta.<n>` suffix;
  the workflow substitutes `{{REL}}` (e.g. `0.8.1-beta.13`), `{{RUNTIME}}`
  (`crates/inka-runtime/runtime-version`), `{{TAG}}` (the exact git tag) and
  `{{CHANNEL}}` before publishing. Do not hardcode versions.

  Keep this to the changes since the PREVIOUS beta only. Do not restate features
  from earlier betas (they are already in those releases); reference an earlier
  release only when context is genuinely needed.
-->
# inka {{REL}} (beta) — runtime tuple {{RUNTIME}}

> **Pre-release.** Published as a GitHub prerelease; the stable channel does not
> install it. `inka update --beta` opts in, `inka update --stable` opts out.

## Install

```sh
# the exact tag (works with any installer, including the current stable one)
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/Cantor-Industries/inka/releases/download/{{TAG}}/install.sh \
  | sh -s -- --version {{TAG}}
```

Windows (preview):

```powershell
irm https://github.com/Cantor-Industries/inka/releases/download/{{TAG}}/install.ps1 -OutFile install.ps1
.\install.ps1 -Version {{TAG}}
```

Already installed? `inka update --beta` moves the toolchain and runtime to the
newest beta.

## Changed since the previous beta

### `inka desktop` packaging on Windows (webview)

`inka desktop <entry> --backend webview` now packages apps on
`x86_64-pc-windows-msvc`, reusing the same shared-engine model as Linux:

- The pinned laufey `webview` backend is downloaded once and checksum-verified,
  renamed to `<App>.exe`, and the app payload is embedded into the co-located
  `<App>.dll` shim (which loads the machine-wide runtime).
- Output is an app directory (`<App>.exe`, `<App>.dll`, `runtime-version`) plus
  a portable `<App>.zip`. There is no `.desktop` entry on Windows.
- `desktop.output.windows` and `desktop.app.icons.windows` are now honored.
- The release now builds the **desktop-enabled** Windows runtime and mirrors the
  pinned Windows laufey backend, so packaging works offline from a release.
- The runtime tuple advances to **{{RUNTIME}}** (the Windows runtime previously
  shipped without the `desktop` feature); `inka update` fetches it.

### Windows `.msi` installer

`inka desktop --installer` (or `-o App.msi`) builds a per-machine Windows
Installer, authored entirely in Rust (no `candle`/`light`): install under
`Program Files\<App>`, an all-users Start Menu shortcut to `<App>.exe`, uninstall
via Add/Remove Programs, and deterministic ProductCode/UpgradeCode across
versions. The app icon (see below) is attached to the shortcut and to
Add/Remove Programs.

### Icons: size sets and a real `.exe` icon

- `desktop.app.icons.windows` may be a `[{ path, size }]` set; inka builds a
  multi-resolution `AppIcon.ico`, writes it beside the app, and embeds it into
  `<App>.exe` (PE resources). A single `.ico`/image works too.
- Linux still ships the largest entry as `AppIcon.png`.

### Deep links

- `--deep-link <scheme>` (repeatable) or `desktop.app.deepLinks` registers URL
  schemes with the OS. Windows emits a `register-deep-links.bat` (run once to
  add `HKCU\Software\Classes\<scheme>`); Linux adds
  `x-scheme-handler/<scheme>;` and `Exec=… %u` to the `.desktop` entry.
- Schemes are validated (RFC 3986); reserved schemes (`http`, `https`, `file`,
  `ftp`, `ws`, `wss`) are rejected.

### Self-extracting app directories

- `--compress` (or `desktop.compress`) replaces the app directory with a thin
  launcher plus a gzip `payload.tar.gz`. The launcher extracts to a per-user
  cache (`%LOCALAPPDATA%\<id>\<hash>` / `${XDG_DATA_HOME:-$HOME/.local/share}/<id>/<hash>`)
  on first run and execs the real app. The `.zip`/`.msi`/`.tar.gz` then wrap the
  compact directory.

### Hardened downloads

- Toolchain and laufey archives are now extracted with traversal and
  zip-symlink refusal, setuid/setgid stripping, and **atomic staging** (extract
  to a sibling temp dir, then rename), so a crash or concurrent build cannot
  leave a half-populated install or cache.

### Artifact payload: embedded `inka` section

- The single-file `inka build` artifact now stores its payload as an embedded
  `inka` binary section instead of an appended `INKFOOT5` trailer. The host
  image stays structurally valid, so an artifact can be Authenticode/`codesign`
  signed after building; `inka doctor` reports `format=inka-section`. Artifacts
  from older releases still run (the launcher reads the legacy trailer as a
  fallback).

## Assets

- `inka-toolchain-{{REL}}-x86_64-unknown-linux-gnu.tar.gz` — CLI + launcher +
  desktop shim (`libinka_desktop_shim.so`)
- `libinka_runtime-{{RUNTIME}}.so` — shared runtime tuple (desktop-enabled)
- `inka-toolchain-{{REL}}-x86_64-pc-windows-msvc.zip` — Windows CLI + launcher +
  desktop shim (`libinka_desktop_shim.dll`)
- `libinka_runtime-{{RUNTIME}}.dll` — Windows shared runtime (**now
  desktop-enabled**)
- `laufey-cef-*.tar.gz` (Linux) and `laufey-webview-*.zip` (Windows) — pinned
  backend mirrors
- `install.sh` / `install.ps1` + `versions.json` — bootstrap installers + version
  record

`.sha256` sidecars are published for the toolchains and runtimes.

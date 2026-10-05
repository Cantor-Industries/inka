<!--
  Maintainer notes. Everything through this closing `-->` is stripped from the
  published body by the release workflow (`.github/workflows/release.yml`).

  Beta release body template. Used when the tag carries a `-beta.<n>` suffix;
  the workflow substitutes `{{REL}}` (e.g. `0.8.1-beta.14`), `{{RUNTIME}}`
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

### `inka desktop` on Windows: the CEF backend, sharing one Chromium runtime

`inka desktop <entry> --backend cef` now packages apps on
`x86_64-pc-windows-msvc`, matching the Linux CEF path:

- The pinned laufey `cef` backend is downloaded once and checksum-verified, and
  the Chromium runtime (`libcef.dll`, `*.pak`, `icudtl.dat`, `locales/`, helper
  executables) is installed once per machine under
  `%LOCALAPPDATA%\cef\<laufey-version>\<target>\`.
- Each app directory gets the runtime as **hard links** into that shared install
  — one physical copy, shared with every CEF app on the machine, and with no
  admin rights required (Windows symlinks need Developer Mode). When linking is
  not possible (a different volume, or a non-NTFS filesystem), inka falls back to
  copying the runtime into the app, exactly as Deno's Windows packager does.
- The release mirrors the pinned Windows `laufey-cef-*.zip`, so CEF packaging
  works offline from a release.

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

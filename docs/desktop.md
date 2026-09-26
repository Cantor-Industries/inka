# Desktop apps

`inka desktop` packages a web app as a native desktop application that **shares
the machine's inka runtime**. The Deno/V8 engine is installed once per user and
every desktop app loads it at start, so the app you ship is a couple of
megabytes instead of the ~150 MB a per-app engine would cost.

```
~/.local/share/inka/runtime/libinka_runtime-<tuple>.so   # shared (desktop-enabled)
~/.local/share/cef/<ver>/<target>/                       # shared CEF runtime (--backend cef)
<App>/                                                    # one packaged app
  <App>            laufey backend (window + renderer), renamed   (Linux)
  <App>.so         per-app shim + your bundled payload           (Linux)
  runtime-version  the shared runtime tuple the shim loads
  AppIcon.png      icon, if configured                           (Linux)
  <id>.desktop     Linux desktop entry                           (Linux)
  libcef.so, ...   symlinks into the shared CEF runtime          (Linux, --backend cef)
<App>.tar.gz

# Windows (%LOCALAPPDATA%\inka\runtime\libinka_runtime-<tuple>.dll, etc.)
<App>\
  <App>.exe        laufey backend (window + renderer), renamed
  <App>.dll        per-app shim + your bundled payload
  runtime-version  the shared runtime tuple the shim loads
  libcef.dll, ...  copied from the shared CEF runtime            (--backend cef)
<App>.zip
```

At launch the prebuilt **laufey backend** loads `<App>.so` (the **shim**), which
unpacks your bundled app to a cache directory, resolves the shared desktop
runtime, `dlopen`s it, and forwards the window/runtime ABI. The engine itself is
never copied into your app.

With `--backend cef` the Chromium runtime is shared per machine: the first CEF
app populates the shared dir (about 360 MB) and every app links those files next
to its launcher. On Linux the shared dir is
`~/.local/share/cef/<laufey-version>/<target>/` and the files are symlinked; on
Windows it is `%LOCALAPPDATA%\cef\<laufey-version>\<target>\` and the files are
**copied** (symlinks are unreliable without developer mode). laufey links
`libcef.so`/`libcef.dll` with `RPATH=.:$ORIGIN` and CEF reads its
`*.pak`/`icudtl.dat`/`locales/` resources from beside the launcher. The launcher
itself stays a real per-app file (laufey derives the runtime library name,
`<App>.so`/`<App>.dll`, from its own path). The shared dir is versioned by the
pinned laufey release; if symlinks are unavailable on the filesystem, inka falls
back to bundling a full copy (always the case on Windows).

Because the app is not self-contained, a runnable app dir (or its default
`<App>.tar.gz`/`<App>.zip`) copied to another machine needs the shared CEF
runtime there too (as with the shared engine) — or set `INKA_CEF_HOME`. On Linux
the portable `--installer` artifacts provision both at install time; on Windows
`--installer` is not supported yet (an MSI is planned), so ship the `.zip`.

The runtime is a normal inka runtime with the `desktop` feature compiled in; the
same `.so` still serves headless `inka run` and built artifacts.

## Requirements

- **Linux `x86_64`** with a system WebKitGTK (the `webview` backend), or
  **Windows `x86_64`** with the Edge **WebView2** runtime (preinstalled on
  Windows 11 and most Windows 10; otherwise the app prompts to install it).
  macOS is not implemented yet.
- The shared **desktop-enabled runtime** (`inka update` installs it; the release
  engine is built with `--features desktop`). Without it, set
  `INKA_DESKTOP_RUNTIME` at launch.
- The **laufey backend** for your platform is downloaded on first use and
  checksum-verified against pinned SHA-256s (laufey `0.7.0`). The `cef` backend
  installs a shared Chromium runtime once and symlinks it into each app (see
  below); `webview` uses the system WebKitGTK.
- For framework mode (`inka desktop`, which defaults to the current directory),
  a **Vite** project.

## Quick start

Your entry must serve HTTP — either a declarative `export default { fetch }`
(auto-serve) or an explicit `Deno.serve`. The shell allocates a loopback port,
runs your server on it, and navigates the window there.

```ts
// main.ts
export default {
  fetch() {
    return new Response("<h1>Hello from inka desktop</h1>", {
      headers: { "content-type": "text/html" },
    });
  },
};
```

```sh
inka desktop main.ts --name Hello   # -> Hello/Hello (+ Hello.tar.gz)
./Hello/Hello
```

Framework mode builds the frontend and serves its `dist/`. With no `entry`,
`inka desktop` packages the current directory (equivalent to `inka desktop .`):

```sh
inka desktop                       # detects Vite in $PWD, runs its build
inka desktop . -o dist/MyApp       # same, with an explicit output directory
```

## Dev mode (`--hmr` and `--inspect`)

`--hmr` and `--inspect*` run the source tree directly through the shared runtime
and the laufey backend — no packaging. The target may be an entry file or a
project directory.

```sh
inka desktop --hmr main.ts         # an entry file: inka's V8 HMR
inka desktop --hmr .               # a framework directory: the framework's HMR
inka desktop --inspect .           # framework production entrypoint + CDP mux
```

For a directory, inka detects the framework and picks a dev-server shape:

- **In-runtime** (`Vite`, `SvelteKit`): the generated entrypoint boots the
  project's Vite dev server *inside* the desktop runtime, so server-side code
  keeps `Deno.desktop`; HMR is Vite's own websocket and the window loads the
  serve port as usual.
- **External** (`Fresh` 2, `Remix`, `React Router`, `Nuxt`, `SolidStart`,
  `TanStack Start`): inka spawns the project's dev command, reads the local URL
  it prints (`Local: http://…`), and navigates the window there. Server-side
  code runs in that separate process, so it does **not** have `Deno.desktop`.
  The command is, in order: `--dev-command`, `package.json` `scripts.dev` (via
  the lockfile's package manager: bun/pnpm/yarn/npm), then `deno task dev`.

`--hmr` on an explicit entry file watches the tree and hot-replaces modules
through V8 (`Debugger.setScriptSource`), reloading the window when a change
can't be applied in place. `--inspect`/`--inspect-brk`/`--inspect-wait` front
the runtime isolate with a CDP mux; use `/deno` via `chrome://inspect`.

## Configuration (`deno.json`)

Put a `desktop` block in `deno.json` (or `deno.jsonc`) to set defaults; CLI
flags always win. `package.json`'s `inka.desktop` is accepted as an
inka-specific fallback.

```jsonc
{
  "version": "1.4.0", // seeds --app-version
  "desktop": {
    "app": {
      "name": "Acme Mail",
      "identifier": "com.acme.mail", // reverse-DNS; .desktop filename + WMClass
      "icons": {
        "linux": "assets/icon-256.png",
        // or a size set (the largest size is used for Linux's single icon):
        // "linux": [{ "path": "assets/icon-32.png", "size": 32 },
        //           { "path": "assets/icon-256.png", "size": 256 }]
      },
    },
    "backend": "webview", // webview (default) | cef | raw
    "output": { "linux": "dist/AcmeMail" },
    "release": { "baseUrl": "https://dl.acme.test/mail" },
    "errorReporting": { "url": "https://errors.acme.test" },
  },
}
```

| Field | Flags equivalent | Effect |
|---|---|---|
| `desktop.app.name` | `--name` | application name (launcher filename + window title) |
| `desktop.app.identifier` | `--identifier` | reverse-DNS id for the `.desktop` file and `StartupWMClass` |
| `desktop.app.icons.linux` | `--icon` | Linux icon (single path, or `[{path,size}]`) |
| `desktop.backend` | `--backend` | `webview`, `cef`, or `raw` |
| `desktop.output.linux` | `-o`/`--output` | output app directory |
| `desktop.release.baseUrl` | `--release-base` | auto-update manifest host |
| `desktop.errorReporting.url` | `--error-reporting` | uncaught-error endpoint |
| top-level `version` | `--app-version` | version reported to `Deno.autoUpdate` |

`output.macos`/`output.windows` and `icons.macos`/`icons.windows` are parsed for
forward compatibility but unused on the Linux-only target. Malformed fields
warn and are ignored; an invalid `identifier` or unknown `backend` is a hard
error.

## Flags

```
inka desktop [entry] [options]

  -o, --output <dir>       output app directory
      --name <name>        application name
      --identifier <id>    reverse-DNS bundle id
      --backend <kind>     webview (default), cef, or raw
      --icon <png>         Linux application icon
      --payload <dir>      use a prebuilt payload directory instead of bundling
      --external <pkg>     leave a package unbundled (embedded from node_modules)
      --no-bundle          copy the entry verbatim as main.js
      --minify             minify the bundle
      --sourcemap          embed an inline source map
      --app-version <ver>  version for Deno.autoUpdate
      --release-base <url> auto-update manifest host
      --error-reporting <url>  POST uncaught errors here
      --installer          also emit <App>.tar.gz + <App>.install.sh
      --engine-base <url>  inka release base for the installer's engine/CEF
```

The entry is bundled with the same resolver as `inka build` (import maps,
`npm:`/`jsr:`, `node_modules`). Use `--payload <dir>` to package an already-built
directory.

## Distribution: the script installer

`inka desktop --installer` adds three files beside the app directory:

```
<App>.tar.gz          app-specific files only (+ an embedded install.sh)
<App>.install.sh      the installer, run standalone
<App>.tar.gz.sha256   checksum the installer verifies when it downloads
```

Unlike the runnable app directory (whose CEF symlinks point into the builder's
home and are therefore not portable), the installer tarball contains **real**
app files only — the launcher, the shim+payload, `runtime-version`, the
`.desktop` entry and icon. The generated `install.sh` reproduces the machine
layout at install time:

- **Shared engine** — if `<XDG_DATA_HOME>/inka/runtime/libinka_runtime-<tuple>.so`
  is already present it is reused; otherwise it is downloaded from the inka
  release the app was built against (its baked tag) and checksum-verified.
- **Shared CEF runtime** (`--backend cef`) — if
  `<XDG_DATA_HOME>/cef/<laufey-version>/<target>/.installed` matches it is
  reused; otherwise the pinned `laufey-cef-<target>.tar.gz` is downloaded from
  the inka release, verified, filtered (no launcher/markers), and installed.
- **App** — extracted to `<XDG_DATA_HOME>/inka/apps/<id>/`, with the CEF
  symlinks created from the shared dir; a `~/.local/bin/<App>` launcher, a
  `.desktop` entry and icon are installed, and `~/.local/bin` is added to
  `PATH` (unless `--no-modify-path`).

```sh
inka desktop . --backend cef --installer
sh MyApp.install.sh                 # or: curl …/MyApp.install.sh | sh
sh MyApp.install.sh --uninstall     # leaves the shared engine/CEF in place
```

The inka release page publishes the matching `versions.json` (with a `laufey`
backends block) and the pinned `laufey-<backend>-<target>.tar.gz` archives, so
the installer never depends on laufey's own host.

## Permissions

Desktop apps ship a small baked permission set rather than the deny-by-default
headless posture: the shim grants your server the loopback address it serves on
and read access to its own unpacked payload. Set `perms=` in the appended
manifest: add a `perms=` line to the appended manifest, or set
`INKA_DESKTOP_PERMS` when launching the runtime directly.

## Auto-update

Point `desktop.release.baseUrl` (or `--release-base`) at a host serving a
`latest.json` and patches, and set `version` / `--app-version`. On launch the
shell applies a staged `<App>.so.update` next to the shim, rolling back to
`.backup` if the new build never reaches its `.update-ok` sentinel. The runtime
provides `Deno.autoUpdate(url, opts)`.

### `latest.json` schema

```jsonc
{
  "version": "1.5.0",
  "patches": {
    // keyed by the *currently installed* version
    "1.4.0": { "name": "app-1.4.0-to-1.5.0.bsdiff", "sha256": "<64 hex>" }
  },
  // Optional, when a `publicKey` is passed to Deno.autoUpdate:
  // `signature` is an ed25519 signature over the UTF-8 bytes of `signed`.
  "signature": "<base64>",
  "signed": "{\"version\":\"1.5.0\",\"patches\":{\"1.4.0\":{\"name\":\"app-1.4.0-to-1.5.0.bsdiff\",\"sha256\":\"...\"}}}"
}
```

- Every patch entry **must** carry `sha256`; the runtime verifies the downloaded
  patch against it (and rejects a mismatch) before applying it.
- With `publicKey`, `signed` must be the canonical manifest as a **string** and
  `signature` the ed25519 signature over its bytes; only the parsed `signed`
  payload is trusted. `Deno.autoUpdate` refuses non-`https` URLs and
  `redirect: "error"`.

## Error reporting

`desktop.errorReporting.url` (or `--error-reporting`) makes the runtime POST
uncaught JS errors **and Rust panics** as JSON (`message`, `stack`,
`appVersion`, `platform`, `arch`). The destination is fixed by the operator at
build time and cannot be retargeted from app code. Only `https://` (or a local
`file://` path) is accepted; plain `http://` is rejected.

## Environment

| Variable | Effect |
|---|---|
| `INKA_DESKTOP_RUNTIME` | path to the shared desktop runtime (overrides discovery) |
| `INKA_DESKTOP_SHIM` | path to the shim for `inka desktop` (default: next to `inka`) |
| `INKA_LAUFEY_BACKEND` | use a specific laufey backend binary |
| `LAUFEY_DEV_DIR` | a laufey source checkout to source the backend from |
| `INKA_LAUFEY_CACHE` | override the laufey backend cache root |
| `INKA_CEF_HOME` | override the shared CEF runtime dir (default `~/.local/share/cef/<ver>/<target>`) |
| `INKA_RELEASE_BASE` | inka release base the installer provisions the engine/CEF from |

## Caveats & roadmap

- **Linux `x86_64` and Windows `x86_64`** (webview backend on both). macOS is
  unimplemented. The Windows webview backend needs the WebView2 runtime.
- The bundled payload is extracted to a per-user cache (`~/.cache/inka/desktop/
  <hash>`) and is not pruned yet.
- HMR (in-runtime Vite, external dev server, and inka's V8 HMR), DevTools
  multiplexing, and framework detection are implemented; `.AppImage`/`.deb`/
  `.rpm` and Windows MSI distribution and Next.js remain follow-ups.
  Distribution today is the runnable directory plus `.tar.gz` (Linux, with the
  script `--installer`) or `.zip` (Windows; `--installer` is not supported yet).
- The per-app `<App>.so`/`<App>.dll` is only the shim + your payload — never the
  engine.
- The `cef` backend relies on user namespaces for Chromium's sandbox on Linux
  (setuid bits are stripped from the downloaded archive, as in Deno). Removing
  the shared CEF runtime breaks packaged CEF apps; `inka doctor` reports its
  presence. macOS CEF packaging is unimplemented.

## Acknowledgments

The desktop runtime adapts Deno's `cli/rt_desktop` and vendored desktop JS
(MIT, Copyright (c) the Deno authors). The window/renderer layer is
[laufey](https://github.com/littledivy/laufey) (MIT, Copyright (c) Divy
Srivastava), pinned at `0.7.0`.

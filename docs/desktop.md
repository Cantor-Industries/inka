# Desktop apps

`inka desktop` packages a web app as a native desktop application that **shares
the machine's inka runtime**. The Deno/V8 engine is installed once per user and
every desktop app loads it at start, so the app you ship is a couple of
megabytes instead of the ~150 MB a per-app engine would cost.

```
~/.local/share/inka/runtime/libinka_runtime-<tuple>.so   # shared (desktop-enabled)
~/.local/share/inka/cef/<ver>/<target>/                  # shared CEF runtime (--backend cef)
<App>/                                                    # one packaged app
  <App>            laufey backend (window + renderer), renamed
  <App>.so         per-app shim + your bundled payload
  runtime-version  the shared runtime tuple the shim loads
  AppIcon.png      icon, if configured
  <id>.desktop     Linux desktop entry
  libcef.so, ...   symlinks into the shared CEF runtime (--backend cef)
<App>.tar.gz
```

At launch the prebuilt **laufey backend** loads `<App>.so` (the **shim**), which
unpacks your bundled app to a cache directory, resolves the shared desktop
runtime, `dlopen`s it, and forwards the window/runtime ABI. The engine itself is
never copied into your app.

With `--backend cef` the Chromium runtime is shared per machine: the first CEF
app populates `~/.local/share/inka/cef/<laufey-version>/<target>/` (about
360 MB) and every app symlinks those files next to its launcher. laufey links
`libcef.so` with `RPATH=.:$ORIGIN` and CEF reads its
`*.pak`/`icudtl.dat`/`locales/` resources from beside the launcher, so the
symlinks keep the app directory at a few megabytes instead of ~360 MB. The
launcher itself stays a real per-app file (laufey derives the runtime library
name, `<App>.so`, from its own path). The shared dir is versioned by the pinned
laufey release; if symlinks are unavailable on the filesystem, inka falls back
to bundling a full copy.

Because the app is not self-contained, a `<App>.tar.gz` unpacked on another
machine needs the shared CEF runtime installed there too (as with the shared
engine) — or set `INKA_CEF_HOME`.

The runtime is a normal inka runtime with the `desktop` feature compiled in; the
same `.so` still serves headless `inka run` and built artifacts.

## Requirements

- **Linux `x86_64`** with a system WebKitGTK (the `webview` backend). macOS and
  Windows are not implemented yet.
- The shared **desktop-enabled runtime** (`inka update` installs it; the release
  engine is built with `--features desktop`). Without it, set
  `INKA_DESKTOP_RUNTIME` at launch.
- The **laufey backend** for your platform is downloaded on first use and
  checksum-verified against pinned SHA-256s (laufey `0.7.0`). The `cef` backend
  installs a shared Chromium runtime once and symlinks it into each app (see
  below); `webview` uses the system WebKitGTK.
- For framework mode (`inka desktop .`), a **Vite** project.

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

Framework mode builds the frontend and serves its `dist/`:

```sh
inka desktop . -o dist/MyApp       # detects Vite, runs its build, serves dist/
```

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
```

The entry is bundled with the same resolver as `inka build` (import maps,
`npm:`/`jsr:`, `node_modules`). Use `--payload <dir>` to package an already-built
directory.

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
| `INKA_CEF_HOME` | override the shared CEF runtime dir (default `~/.local/share/inka/cef/<ver>/<target>`) |

## Caveats & roadmap

- **Linux only.** macOS/Windows bundling and backends are unimplemented.
- The bundled payload is extracted to `~/.cache/inka/desktop/<hash>` and is not
  pruned yet.
- HMR, DevTools multiplexing, and `.AppImage`/`.deb`/`.rpm` distribution are
  follow-ups; the packaged app is a directory plus a `.tar.gz`.
- The per-app `<App>.so` is only the shim + your payload — never the engine.
- The `cef` backend relies on user namespaces for Chromium's sandbox (setuid
  bits are stripped from the downloaded archive, as in Deno). Removing the
  shared CEF runtime (`~/.local/share/inka/cef/...`) breaks packaged CEF apps;
  `inka doctor` reports its presence. macOS/Windows CEF packaging is
  unimplemented.

## Acknowledgments

The desktop runtime adapts Deno's `cli/rt_desktop` and vendored desktop JS
(MIT, Copyright (c) the Deno authors). The window/renderer layer is
[laufey](https://github.com/littledivy/laufey) (MIT, Copyright (c) Divy
Srivastava), pinned at `0.7.0`.

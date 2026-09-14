# CLI reference

```
inka build   [source] [-s <file>] [-o <file>] [--runtime <spec>] [--tested-against <ver>] [-A|--allow-all] [-R|-W|-N|-E|-S[=list]] [--allow-<cat>[=list]] [--deny-<cat>[=list]] [-P[=<set>]] [--minify] [--sourcemap] [--external[=<pkg>]]... [--embed-dir]
inka run     [-A] [-P[=name]] [--allow-<cat>[=list]|--deny-<cat>[=list]]... <file> [args...]
inka update  [<version>] [--from <dir-or-url>] [--sha256 <hex>] [--insecure] [--home <dir>]
             [--no-toolchain|--toolchain-only] [--no-runtime]
inka list    [--home <dir>]
inka doctor
inka --version, -V
```

## `build`

Bundle the entry (import maps + `npm:`/`jsr:` + `node_modules`) into one
self-contained module, then pack it onto the launcher with a manifest.
Permissions bake from explicit build-intent sources — CLI flags (`-A`/`--allow-*`,
`-P=<set>`, which override config), `deno.json` `compile.permissions`, or an
`inka.permissions` marker in `deno.json`/`package.json`; otherwise the artifact
is deny-by-default. See [Build](build.md).

## `run`

Execute a `.ts`/`.js` file through the installed runtime without building.
Permissions are deny-by-default; `-A` allows all, `-P[=name]` applies a named
config set, `--allow-<cat>[=list]` / `--deny-<cat>[=list]` are granular
(`cat`: `read|write|net|env|run|sys|ffi|import`). `--runtime <ver>` picks a specific
tuple; `--` ends options. See [Run](run.md).

## `update`

Reconcile the toolchain and shared runtime with the release channel:

- no version: read `<base>/versions.json`; self-update the toolchain when an
  installer-managed install is present (`VERSION` marker), and install the
  runtime when it is missing or behind. Never downgrades; older runtime tuples
  are kept.
- `<version>`: install that exact runtime tuple (offline/pinned).

`--no-toolchain`/`--toolchain-only` control the toolchain; `--no-runtime` skips
the runtime.

Base resolution: `--from` → `$INKA_RELEASE_BASE` → `$INKA_RT_SOURCE` → the
built-in GitHub latest-release URL. `--sha256` pins a checksum; `--insecure`
skips verification; `--home <dir>` sets the runtime install dir. See
[Install & upgrade](install-and-upgrade.md).

## `list` / `doctor`

`list` prints installed runtime tuples (across all search dirs; `--home` narrows
to one). `doctor` prints a diagnostic report: installed runtimes plus project
status (config files, `node_modules`, `DENO_DIR`, bundling capability, launcher).

## Environment

| Variable | Effect |
|---|---|
| `XDG_DATA_HOME` | base for `inka/runtime` (default `~/.local/share`) |
| `INKA_RUNTIME_HOME` | override the per-user runtime dir |
| `DENO_DIR` | Deno cache read for `jsr:`/remote resolution (default `~/.cache/deno`); must be an absolute path — the cache is **trusted input** (cached remote/JS is loaded as code) |
| `INKA_RELEASE_BASE` / `INKA_RT_SOURCE` | override the update channel base |
| `INKA_LAUNCHER` | path to `inka-launcher` for `build` |
| `INKA_DEBUG` | verbose runtime/resolution logging |

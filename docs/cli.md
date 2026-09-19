# CLI reference

```
inka build   [source] [-s <file>] [-o <file>] [--runtime <spec>] [--tested-against <ver>] [-A|--allow-all] [-R|-W|-N|-E|-S[=list]] [--allow-<cat>[=list]] [--deny-<cat>[=list]] [-P[=<set>]] [--minify] [--sourcemap] [--external[=<pkg>]]... [--embed-dir] [--path-base <exe|cwd>] [--fetch] [--beta|--stable]
inka run     [-A] [-P[=name]] [--allow-<cat>[=list]|--deny-<cat>[=list]]... [--path-base <exe|cwd>] [--fetch] [--beta|--stable] <file> [args...]
inka cache   <file>
inka desktop [entry] [-o <dir>] [--name <name>] [--identifier <id>] [--backend <kind>] [--icon <png>] [--payload <dir>] [--external[=<pkg>]]... [--no-bundle] [--minify] [--sourcemap] [--app-version <ver>] [--release-base <url>] [--error-reporting <url>]
inka update  [<version>] [--from <dir-or-url>] [--sha256 <hex>] [--insecure] [--home <dir>]
             [--no-toolchain|--toolchain-only] [--no-runtime] [--beta|--stable]
inka doctor  [artifact] [--json] [--beta|--stable]
inka help    [command]
inka --version, -V
```

## `build`

Bundle the entry (import maps + `npm:`/`jsr:` + `node_modules`) into one
self-contained module, then pack it onto the launcher with a manifest.
Permissions bake from explicit build-intent sources — CLI flags (`-A`/`--allow-*`,
`-P=<set>`, which override config), `deno.json` `compile.permissions`, or an
`inka.permissions` marker in `deno.json`/`package.json`; otherwise the artifact
is deny-by-default.

The **toolchain channel** decides the default: a stable release builds a stable
artifact, a `-beta.N`/`-rc.N` release builds a beta one, and an unbaked dev build
defaults to stable. `--beta`/`--stable` override it, and `INKA_CHANNEL` sits
between the flag and the default. `--beta` is **refused on a stable release**
(`run \`inka update --beta\`` to switch). A prerelease `--runtime`/
`--tested-against` spec records `channel=beta` on its own, so the artifact may
select a prerelease runtime tuple. See [Build](build.md).

## `run`

Execute a `.ts`/`.js` file through the installed runtime without building.
Permissions are deny-by-default; `-A` allows all, `-P[=name]` applies a named
config set, `--allow-<cat>[=list]` / `--deny-<cat>[=list]` are granular
(`cat`: `read|write|net|env|run|sys|ffi|import`). `--runtime <ver>` picks a specific
tuple; the toolchain channel selects the default tuple, with `--beta`/`--stable`
overriding it. A prerelease tuple requires the beta channel: on a stable
toolchain both `--beta` and a prerelease `--runtime` are refused. `--` ends
options. See [Run](run.md).

## `cache`

`inka cache <file>` fetches the remote (`jsr:`/`https:`) modules in the file's
import graph that are missing from the Deno cache (`$DENO_DIR`), so later
**offline** builds/runs resolve them. This is the explicit network opt-in;
`inka build --fetch` and `inka run --fetch` do the same warm-up then continue.
Requires a build with bundling support. See
[Dependencies & resolution](packages.md).

## `desktop`

Package a web app as a native desktop application that shares the machine's
runtime. The entry must serve HTTP (`export default { fetch }` or
`Deno.serve`); `inka desktop <entry>` bundles it, and `inka desktop .` detects
and builds a Vite project. Defaults come from `deno.json`'s `desktop` block
(`app.name`/`app.identifier`/`app.icons`/`backend`/`output`/`release.baseUrl`/
`errorReporting.url`, plus top-level `version`); CLI flags override them. The
output is an app directory, a `.desktop` entry, and a `.tar.gz`. Linux only
today. See [Desktop apps](desktop.md).

## `update`

Reconcile the toolchain and shared runtime with the release channel:

- no version: read `<base>/versions.json`; self-update the toolchain when an
  installer-managed install is present (`VERSION` marker), and install the
  runtime when it is missing or behind. Never downgrades; older runtime tuples
  are kept.
- `<version>`: install that exact runtime tuple (offline/pinned).

`--no-toolchain`/`--toolchain-only` control the toolchain; `--no-runtime` skips
the runtime.

`--beta` installs the newest beta release (toolchain + runtime); `--stable`
returns to the stable channel. `update` is the channel **switcher** and never
refuses beta. With no flag, the channel follows the toolchain's own channel, so
plain `inka update` on a beta toolchain stays on beta (no downgrade); an
explicit `--from`/`INKA_RELEASE_BASE` instead defers to the staged
`versions.json.channel` unless a flag/env channel was given. The beta base is
resolved from the GitHub Releases API (`INKA_REPO`, default
`Cantor-Industries/inka`).

Base resolution: `--from` → `$INKA_RELEASE_BASE` → `$INKA_RT_SOURCE` → the
built-in GitHub latest-release URL (or, for a beta toolchain / `--beta`, the
newest beta). `--sha256` pins a checksum; `--insecure` skips verification;
`--home <dir>` sets the runtime install dir. See
[Install & upgrade](install-and-upgrade.md).

## `doctor`

`doctor` with no argument prints a diagnostic report: the toolchain
(`release (short-hash)` when baked, plus its `channel`), installed runtimes plus
the selected tuple's advertised capabilities (e.g. `desktop` on a
desktop-enabled engine), project status (config files, `node_modules`,
`DENO_DIR`, bundling capability, launcher, desktop shim). The report is grouped
into sections with status glyphs (`✓` ok, `!` warning, `✗` problem), marks the
tuple the effective channel would select, and prints a `hint:` for each problem.
It is informational and always exits `0`.

`doctor <artifact>` instead **inspects an inka executable**: its module, runtime
floor and `tested-against` cap, `requires=`, `path-base`, baked permission
lines, and embedded-file count/sizes, plus whether a compatible runtime is
installed and which tuple would be selected. `--json` prints the same as JSON.
A non-artifact path is a hard error (exit `2`); a malformed constraint or no
compatible runtime exits `3`. See [Troubleshooting](troubleshooting.md).

## `help`

`help [command]` prints the full help for a command; `-h` prints a short
summary and `--help` the full one. Running `inka` with no arguments prints the
top-level help and exits `0`.

Output is styled when a stream is a TTY; piped/CI output is plain. `-q`/
`--quiet` and `-v`/`--verbose` adjust verbosity, and `build`/`run`/`update`
honor them.

## Environment

| Variable | Effect |
|---|---|
| `XDG_DATA_HOME` | base for `inka/runtime` (default `~/.local/share`) |
| `INKA_RUNTIME_HOME` | override the per-user runtime dir |
| `DENO_DIR` | Deno cache read for `jsr:`/remote resolution (default `~/.cache/deno`); must be an absolute path — the cache is **trusted input** (cached remote/JS is loaded as code) |
| `INKA_RELEASE_BASE` / `INKA_RT_SOURCE` | override the update channel base |
| `INKA_CHANNEL` | `stable` or `beta` (strict; an unknown value is a hard error). Sits between `--beta`/`--stable` and the toolchain default |
| `INKA_REPO` | `owner/repo` for beta discovery (default `Cantor-Industries/inka`) |
| `INKA_GITHUB_TOKEN` / `GITHUB_TOKEN` | token for the GitHub Releases API (raises the rate limit) |
| `INKA_LAUNCHER` | path to `inka-launcher` for `build` |
| `INKA_DESKTOP_RUNTIME` | shared desktop runtime path for packaged apps (`inka desktop`) |
| `INKA_DESKTOP_SHIM` | per-app shim path for `inka desktop` (default: next to `inka`) |
| `INKA_LAUFEY_BACKEND` / `LAUFEY_DEV_DIR` | laufey backend binary / source checkout for `inka desktop` |
| `INKA_LAUFEY_CACHE` | laufey backend cache root (default `~/.cache/inka/laufey`) |
| `INKA_DEBUG` | verbose runtime/resolution logging |
| `INK_LOG` | log level: `error`\|`warn`\|`info`\|`debug`\|`trace` (default `info`) |
| `INK_LOG_STYLE` | color: `auto` (default; TTY only), `always`, `never` |

Colored output also follows the de-facto `NO_COLOR` (disable) and `FORCE_COLOR`
(enable) variables.

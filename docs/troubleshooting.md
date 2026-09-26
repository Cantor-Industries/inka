# Troubleshooting

Start with `inka doctor` — it prints the runtime dirs, installed runtimes, and
project status (config files, `node_modules`, `DENO_DIR`, bundling capability,
launcher), grouped into sections with a status glyph per row and a `hint:` for
each problem. The report is colored only on a TTY (honors `NO_COLOR`) and
always exits `0`.

To diagnose a **built executable**, pass it: `inka doctor ./app` locates the
embedded `inka` section (falling back to the legacy `INKFOOT5` trailer) and
reports the module, runtime floor/`tested-against` cap, `requires=`,
`path-base`, baked permission lines, and embedded payload, plus whether a
compatible runtime is installed. The `format` row names the detected layout
(`inka-section` or `INKFOOT5`). Unlike machine mode it exits nonzero on trouble:
`2` if the path is not an inka artifact, `3` if the manifest constraint is
malformed or no compatible runtime is installed. `--json` emits the same data
for scripts.

## Exit codes

Built artifacts (launcher):

| Code | Meaning |
|---|---|
| `2` | not an inka artifact / corrupt payload section or trailer |
| `3` | no compatible runtime installed, or an unparseable manifest version constraint |
| `4` | installed runtime is too old for the ABI, is missing a required symbol, reports a version that contradicts its filename, or lacks a required capability (`requires=`) |
| other | the program's own exit code (propagated) |

`inka run` uses `2` for bad flags/usage, `1` for load failures, and `4` for a
runtime missing `inka_runtime_create` / `inka_runtime_run_module_dir` (or whose
reported version does not match its filename). `inka build` also exits `2` on
usage errors.

Runtime selection searches `$INKA_RUNTIME_HOME` and the per-user XDG runtime
dir; there is no per-invocation runtime override.

## Common problems

**"no runtime installed" / exit 3.** Run `inka update`. The launcher searches
`$INKA_RUNTIME_HOME` and `~/.local/share/inka/runtime`. If the artifact pins
`tested-against`, a newer runtime is not selected — install one within range.

**`NotCapable` / permission errors.** inka artifacts are deny-by-default. Bake
grants at build time (`inka build -A`, `--allow-*`, `-P=<set>`, or an
`inka.permissions` marker) or grant them per run (`inka run -A`, `--allow-*`,
`-P=<set>`). See [Permissions](permissions.md). `inka build` warns when no
permission source was found.

**Build fails: package not found.** `inka` is offline. Install dependencies with
your package manager (so they are in `node_modules`), and for `jsr:` run
`deno cache`/`deno install` first so the module is in `DENO_DIR`.

**`Could not find package 'src'` (or another first path segment).** The import is
a `tsconfig.json` `baseUrl`/`paths` alias (e.g. `import "src/util"` with
`"baseUrl": "."`), not an npm package. `inka run` reads the importing file's
nearest `tsconfig.json`/`jsconfig.json` (the same resolver `inka build` uses), so
this works for ESM imports. If it still fails, check that the config is an
ancestor of the importing file, that the path exists, and that the file is
inside the execution tree. `require()` of a `baseUrl` path is not resolved (use
an ESM import).

**`jsr:` works at build time but not at run time.** `inka build` inlines bundled
`jsr:` code, so the artifact needs no cache. If you passed `--external`, the
files are embedded instead. `inka run` resolves `jsr:` from `DENO_DIR` live, so an
uncached package errors.

**Native `.node` addon fails.** Native addons must be left external
(`inka build --external <pkg>`) so their files are embedded, and the artifact must
grant `ffi` (plus `sys` for platform detection) at run time.

**`ReferenceError: <name> is not defined` from inside `eval`.** A source module
uses **direct `eval`** whose string references a module-scope binding (a local,
an imported name like `factory`/`ts`, …). `inka run` gives each module its own
scope, so it works; a bundled artifact scope-hoists every module into one scope
and role inlines/renames imports, so direct eval cannot see those names. `inka
build` prints rolldown's `Use of direct `eval` …` warning for the affected
modules. Fixes: pass the names explicitly with `new Function("dep", "return " +
code)(dep)`, use indirect eval `(0, eval)(code)` (global scope only), or keep the
package out of scope hoisting. See
<https://rolldown.rs/guide/troubleshooting#avoiding-direct-eval>.

**"inka was built without bundling support".** The `inka` binary was compiled
without the `bundle` feature. Use an official release, or build with
`cargo build --release -p inka --features bundle`.

**`inka update` can't reach the channel.** Set `INKA_RELEASE_BASE` (or pass
`--from`) to a reachable release base; `inka update <ver> --from <dir>` works
fully offline against a local directory.

**Coming from 0.4.x (or older) to 0.5.0.** 0.5.0 is a clean break: the package
store and vendoring were removed, `inka build` now bundles, and the runtime tuple
moved to `0.266.2`. `inka update` will not cross this boundary — re-run
`install.sh` (the same command you installed with). It detects the pre-0.5.0
install and resets the old toolchain and runtime before provisioning fresh.

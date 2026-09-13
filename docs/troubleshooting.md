# Troubleshooting

Start with `inka doctor` — it prints the runtime dirs, installed runtimes, and
project status (config files, `node_modules`, `DENO_DIR`, bundling capability,
launcher), plus any warnings.

## Exit codes

Built artifacts (launcher):

| Code | Meaning |
|---|---|
| `2` | not an inka artifact / corrupt trailer |
| `3` | no compatible runtime installed for the manifest |
| `4` | installed runtime is too old for the ABI the artifact needs |
| other | the program's own exit code (propagated) |

`inka run` uses `2` for bad flags/usage, `1` for load failures, and `4` for a
runtime missing `inka_runtime_create` / `inka_runtime_run_module_perm` /
`inka_runtime_run_module_dir`.

## Common problems

**"no runtime installed" / exit 3.** Run `inka update`. The launcher searches
`$INKA_RUNTIME_HOME` and `~/.local/share/inka/runtime`. If the artifact pins
`tested-against`, a newer runtime is not selected — install one within range.

**`NotCapable` / permission errors.** inka artifacts are deny-by-default. Grant
access with `-A`, `-P`, or granular `--allow-*` (see [Permissions](permissions.md)).

**Build fails: package not found.** `inka` is offline. Install dependencies with
your package manager (so they are in `node_modules`), and for `jsr:` run
`deno cache`/`deno install` first so the module is in `DENO_DIR`.

**`jsr:` works at build time but not at run time.** `inka build` inlines bundled
`jsr:` code, so the artifact needs no cache. If you passed `--external`, the
files are embedded instead. `inka run` resolves `jsr:` from `DENO_DIR` live, so an
uncached package errors.

**Native `.node` addon fails.** Native addons must be left external
(`inka build --external <pkg>`) so their files are embedded, and the artifact must
grant `ffi` (plus `sys` for platform detection) at run time.

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

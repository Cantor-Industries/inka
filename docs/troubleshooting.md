# Troubleshooting

Start with `inka doctor` — it prints the runtime dirs, installed
runtime/resolver (+ ABI), the default store (packages + seed `sha256`), the
vendored pool, and any warnings.

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

**"no inka resolver installed".** `inka update` installs the resolver. You can
also point `INKA_RESOLVER` at a specific `libinka_resolver-<v>.so`.

**Store missing / `packages=0`.** `inka update` seeds the default store. If
`inka doctor` warns that `vendored.lock` was built against a different store,
re-run `inka update` or re-vendor the affected packages.

**Artifact runs on one machine but not another.** The dependency came from the
default store, which differs per machine. Vendor the closure (`inka install`,
`inka build --vendor-closure`) or ensure both machines share the same store
identity (`inka doctor`).

**`inka update` can't reach the channel.** Set `INKA_RELEASE_BASE` (or pass
`--from`) to a reachable release base; `inka update <ver> --from <dir>` works
fully offline against a local directory.

**`inka update` didn't upgrade the toolchain from 0.2.2 to 0.3.0.** The 0.2.2
self-updater expects an `inka-patcher` binary that 0.3.0 no longer ships, so it
warns and leaves the old CLI in place. Re-run `install.sh` (the same command you
installed with) to replace the toolchain; the runtime, resolver, and store were
already updated. From 0.3.0 on, `inka update` self-updates normally.

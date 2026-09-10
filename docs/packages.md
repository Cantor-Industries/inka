# Packages & the store

inka resolves `npm:`/`jsr:` (and bare) imports from two tiers, checked in order:

1. **Project `vendored/`** — package roots committed or ignored with your project
   (see `inka vendor release|ignore`). Self-contained and version-pinned.
2. **Machine default store** — `~/.local/share/inka/store` (`$INKA_STORE`
   overrides), a shared hoisted `node_modules` pool seeded from a release
   snapshot and kept current by `inka update`.
3. **Built-ins** — Node built-ins served by the engine.

Store-internal imports never consult `vendored/`; vendored code resolves
vendored-first, then the store.

## The default store

The store is a normal npm layout (`node_modules/` + `seed-manifest.json`). It is
provisioned automatically:

- the toolchain `.deb` bundles a snapshot when it changed and seeds it on
  install (see [Install & upgrade](install-and-upgrade.md));
- `inka update` fetches `seed-manifest.json` + `store.tar.gz` from the release
  channel and replaces `node_modules` when the recorded `sha256` differs.

Nothing installs or runs on the consumer machine — the snapshot is built once,
at release time, with pre/postinstall already applied.

## Declaring dependencies

`package.json` `dependencies` and `deno.json` `imports` are the project's root
set. Vendor all of them, or individual packages:

```sh
inka install                 # vendor every declared root
inka install zod@3.23.8      # vendor specific packages (like `inka add`)
inka add nanoid              # vendor one package
inka remove nanoid           # un-vendor (+ prune orphaned vendored deps)
```

- If the default store already provides the exact resolved version, nothing is
  vendored (two-tier dedupe). Use `--force` to vendor anyway.
- Ranges (`^1.2`, `~1.2.3`, `>=…`) are resolved via npm and pinned to the
  resolved exact version.
- `jsr:@scope/pkg` is stored under its npm-mirror identity `@jsr/scope__pkg`.

## CommonJS conversion

Packages whose import-reachable entry is CommonJS are converted to engine-viable
ESM when vendored/snapshotted. Curated specs live under
`patches/<pkg>/<version>/patch.json` and are applied by the sibling
`inka-patcher` binary; hard cases error with the exact spec to create.

## Inspecting

```sh
inka vendor list     # vendored roots
inka vendor status   # vendored set + default-store coverage, lock drift
inka doctor          # store path, package count, seed sha, vendored pool
```

`vendored.lock` records the default-store identity used at vendor time;
`inka doctor` warns (never fails) when the current store differs.

## Building offline / portable

- Vendor the whole closure (`inka install` with the store absent, or
  `inka build --vendor-closure`) so the artifact carries its dependencies.
- `inka build --no-vendor` relies on the machine default store at run time.

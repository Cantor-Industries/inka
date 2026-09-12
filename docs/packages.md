# Packages & the store

inka resolves `npm:`/`jsr:` (and bare) imports from these tiers, checked in
order:

1. **Project `vendored/`** — a real npm `node_modules` tree
   (`vendored/node_modules/…`) committed or ignored with your project (see
   `inka vendor release|ignore`). Self-contained and version-pinned; it shadows
   the tiers below.
2. **Project `node_modules`** — if your project already has one (created by
   `npm`, `yarn`, `pnpm`, or `deno install`), inka resolves from it
   (bring-your-own-node_modules). Hoisted, nested (conflicting versions), and
   symlinked layouts (Deno isolated `.deno/`, pnpm `.pnpm/`) all work.
3. **Machine default store** — `~/.local/share/inka/store` (`$INKA_STORE`
   overrides), a shared hoisted `node_modules` pool seeded from a release
   snapshot and kept current by `inka update`.
4. **Built-ins** — Node built-ins served by the engine.

Within a tree the **nearest `node_modules` wins** (a nested version beats a
hoisted one). Store-internal imports never consult project tiers; project code
resolves vendored → project `node_modules` → store.

## Bring your own `node_modules`

If a project already has a `node_modules` directory, `inka run` resolves bare
imports from it with no vendoring step. `inka build` **auto-embeds** the
reachable `node_modules` graph into the artifact so it stays self-contained;
pass `--no-node-modules` to leave dependencies to the machine store instead.

## The default store

The store is a normal npm layout (`node_modules/` + `seed-manifest.json`). It is
provisioned automatically:

- `install.sh` seeds it on first install (it runs `inka update`; see
  [Install & upgrade](install-and-upgrade.md));
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
inka remove nanoid           # un-vendor a package
```

`vendored/` holds a real npm `node_modules` tree. `inka add`/`install`/`remove`
re-resolve the whole root set with one `npm install`, so transitive version
conflicts are **nested** rather than rejected:

```
vendored/node_modules/ms/                       # hoisted (a root)
vendored/node_modules/debug/node_modules/ms/    # nested (a conflict)
```

- If the default store already provides the exact resolved root, nothing is
  vendored (root-level dedupe). Use `--force` to vendor anyway.
- Ranges (`^1.2`, `~1.2.3`, `>=…`) are resolved via npm and pinned to the
  resolved exact version in your manifests and `vendored.lock`.
- `jsr:@scope/pkg` is stored under its npm-mirror identity `@jsr/scope__pkg`.

## CommonJS

The engine runs CommonJS natively, so vendored, project `node_modules`, and
store packages are shipped exactly as npm resolves them — no conversion step.
`require()` works inside CJS packages (including nested deps and cycles), and
ESM `import` of a CJS package is served as an ESM facade with `default` plus
statically-detected named exports.

Native `.node` (N-API) addons load from the store or a project-local
`node_modules`/vendored tree, but they are **opt-in**: deny-by-default still
applies, so the artifact or `inka run` must grant `ffi` for the addon path (and
`sys` for platform detection, e.g. `detect-libc`). Without it, requiring an
addon fails with a clean `NotCapable` rather than crashing. For example:

```sh
inka run --allow-sys --allow-ffi app.ts   # or bake the same grants at build time
```

## Inspecting

```sh
inka vendor list     # vendored roots + node_modules package count
inka vendor status   # vendored roots + default-store coverage, lock drift
inka doctor          # store path, package count, seed sha, vendored pool
```

`vendored.lock` (format v2) records the declared roots and the default-store
identity used at vendor time; `inka doctor` warns (never fails) when the current
store differs.

## Building offline / portable

- `inka build` embeds the project `node_modules` closure and the `vendored/`
  tree automatically. `--no-node-modules` and `--no-vendor` opt out (the
  artifact then resolves from the machine store at run time).
- `inka build --vendor-closure` embeds only the vendored modules reachable from
  the entry graph.

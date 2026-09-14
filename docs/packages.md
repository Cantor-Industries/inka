# Dependencies & resolution

inka does **no package management**. It expects an existing project managed by
any package manager (npm, pnpm, yarn, bun, or Deno) and resolves/bundles from
what is already on disk.

## Resolution order

`inka build` and `inka run` resolve imports from:

1. **Import map** — `deno.json`/`deno.jsonc` `imports`/`scopes` (bare specifiers
   and remaps).
2. **`tsconfig.json`/`jsconfig.json` `baseUrl`/`paths`** — a bare specifier like
   `src/util` (with `"baseUrl": "."`) or an alias like `@/*` resolves against
   the **nearest** config for the importing file (the same lookup TypeScript and
   Bun use, including `extends` and JSONC comments). Results are confined to the
   execution tree. Applies to ESM imports; `require()` of a `baseUrl` path is
   not resolved (deno's CommonJS loader does not consult tsconfig) — use an ESM
   import for those.
3. **Project `node_modules`** — bring-your-own-node_modules. Hoisted, nested
   (conflicting versions), and symlinked layouts (Deno isolated `.deno/`, pnpm
   `.pnpm/`) all work; the **nearest `node_modules` wins**. In a monorepo where
   deps are hoisted to a workspace root, both commands climb to the ancestor
   `node_modules` (the run root is the nearest ancestor that is a project root
   and has a `node_modules`, or the workspace root itself).
4. **Deno cache (`DENO_DIR`)** — `jsr:` modules (and any cached remote `https:`)
   are read from `$DENO_DIR/remote`; `npm:` resolves against `node_modules`.
5. **Built-ins** — `node:` modules served by the engine.

`inka` is **offline**: it never fetches from the network. If a `jsr:` package is
not in the Deno cache, run `deno cache`/`deno install` first.

## `inka build` bundles

`inka build` walks the entry graph (import maps, bare, `npm:`, `jsr:`, `node:`)
and bundles it with [rolldown](https://rolldown.rs) into one self-contained ESM
module, then packs it onto the launcher. TypeScript is transpiled; tree-shaking
is on. Options:

- `--minify` — minify the bundle.
- `--sourcemap` — embed an inline source map.
- `--external <pkg>` — leave a package **unbundled** but embed its files from
  `node_modules` (the artifact resolves it from the extracted tree at run time).
  The package's **transitive dependency closure** is embedded too, so hoisted
  (npm/yarn/bun) and symlinked isolated (pnpm `.pnpm/`, Deno `.deno/`) layouts
  both work; conflicting versions are nested under the referring package.
- `--embed-dir` — also embed the whole current-directory tree (for assets).

Everything else (import maps, `npm:`, `jsr:`, `node_modules`) is inlined, so the
artifact needs no `node_modules` or Deno cache at run time.

## `inka run` executes directly

`inka run` executes a `.ts`/`.js` entry through the installed runtime without
building. It resolves import maps, bare `node_modules`, `npm:` (node_modules),
and `jsr:` (Deno cache, offline). Uncached `jsr:` packages are a clear error.

## CommonJS

The engine runs CommonJS natively. `require()` works inside CJS packages
(including nested deps and cycles), and ESM `import` of a CJS package is served
as an ESM facade with `default` plus statically-detected named exports. `npm:`
packages are taken from `node_modules` exactly as npm resolves them.

## Native addons (`.node`)

Native addons cannot be bundled, so leave the package external so its files are
embedded:

```sh
inka build app.ts --external @parcel/watcher
```

They are **opt-in** at run time: deny-by-default still applies, so grant `ffi`
for the addon path (and usually `sys` for platform detection such as
`detect-libc`). Without it, loading the addon fails with a clean `NotCapable`
rather than crashing.

```sh
inka run --allow-sys --allow-ffi app.ts   # or bake the grants at build time
```

## Declaring dependencies

inka does **not** read `package.json` `dependencies` or `deno.json` `imports`
for the manifest — the bundler/resolver consume those to build the module graph.
For the **permission/runtime manifest** it reads only the permission config
(top-level `permissions`, `deno.json` `compile.permissions`, and the `inka`
block). It never installs anything. Declare and install dependencies with your
package manager as usual:

```sh
npm install zod
inka build app.ts      # zod is bundled
```

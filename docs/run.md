# `inka run`

Executes a `.ts`/`.js`/`.mjs`/`.cts` file directly through the installed
runtime — no artifact build. Imports (relative, `deno.json` import maps, the
project's `node_modules`, `npm:` via `node_modules`, and `jsr:` from the Deno
cache) resolve exactly as they would in a built artifact, and `.ts` is
transpiled at load.

```sh
inka run [options] <file> [args...]
```

Options must precede the file; anything after it (or after `--`) is passed to
the program as its arguments, and the program's exit code is propagated.

## Permissions

`inka run` mirrors `deno run --no-prompt`: **deny by default**, explicit grants
only, no prompting. This table uses the runtime's categories (`read`, `write`,
`net`, `env`, `run`, `sys`, `ffi`, `import`).

| Flag | Meaning |
|---|---|
| `-A`, `--allow-all` | allow everything (trimmed by any `--deny-*`) |
| `-R` `-W` `-N` `-E` `-S` | allow read/write/net/env/sys (whole category); `-R=./data` etc. scopes it |
| `--allow-<cat>[=list]` | grant a category (no value = whole category) |
| `--deny-<cat>[=list]` | deny within an allowed category |
| `-P`, `-P=<name>`, `--permission-set <name>` | apply a named permission set from the config (bare `-P` = the `default` set) |
| `--runtime <ver>` | use a specific installed runtime tuple instead of the newest |
| `--path-base <exe\|cwd>` | anchor relative read/write grants to the entry dir (`exe`) instead of the cwd |
| `--fetch` | fetch missing remote (`jsr:`/`https:`) modules into the cache first |

`allow-read`/`allow-write` values may contain the `${EXE_DIR}` (entry dir) and
`${PROJECT_DIR}` (execution root) tokens — see
[Permissions](permissions.md#portable-grants-tokens-and-path-base).

Examples:

```sh
inka run app.ts                          # deny-by-default
inka run -A app.ts                       # allow everything
inka run -R=./data app.ts                # read ./data only
inka run --allow-net=api.example.com app.ts
inka run -P=server app.ts                # the config's `server` set
inka run -A --deny-read=./secrets app.ts # allow-all, minus secrets
inka run -- app-with-dashes.js           # end options; file may start with '-'
```

Rules:

- `-A` cannot combine with `-P` or `--allow-*` (deny-* may trim it); `-P`
  cannot combine with granular flags.
- `--deny-*` requires an allow source (`-A` or `--allow-*`); a deny with no
  allow is an error.
- Only `-P` is honored for `run` — `compile.permissions` and auto-defaults are
  never applied (that's a *build* concept). If the config has
  `compile.permissions` or an `inka.permissions` marker, `run` prints a hint
  naming the `-P=<set>` that reproduces what a build would bake. Use `-P=<name>`
  (or `--permission-set=<name>`); a bare `-P` selects the `default` set.
- Repeated allow/deny flags merge per category (`*` wins, lists join).
- If you run with no permission flags and your config declares a non-empty
  `permissions.default`, inka prints a hint reminding you the run is
  deny-by-default.

## Execution root

The entry resolves like a build: a file under the current directory is rooted at
the current directory; an **outside-cwd file** is rooted at its nearest ancestor
project (a `package.json` or `deno.json`) or its own directory otherwise.
Relative imports stay bounded by that root.

Inside a **workspace** (an ancestor `package.json` with `workspaces`, or a
`deno.json` `workspace`), the root is the workspace root instead, so sibling
packages (symlinked into `node_modules`) stay inside the execution tree and
resolve as they do in `inka build`. When the current directory has no
`node_modules` but an ancestor workspace root does (hoisted dependencies), `run`
also roots at that ancestor.

Bare specifiers resolve in this order: the `deno.json` import map (plus
`package.json` `#imports` and workspace members), then the importing file's
nearest `tsconfig.json`/`jsconfig.json` `compilerOptions.baseUrl`/`paths`, then
the project's `node_modules`. The tsconfig step uses the same resolver as
`inka build`, so a `"baseUrl": "."` `src/util` import or a `@/*` alias works in
`run` exactly as it does in a built artifact. `require()` of a `baseUrl` path is
not supported (use an ESM import).

## See also

- [Permissions](permissions.md) — the model behind the flags
- [`inka build`](build.md) — baking permissions into an artifact
- [Troubleshooting](troubleshooting.md) — exit codes and errors

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
`net`, `env`, `run`, `sys`, `ffi`).

| Flag | Meaning |
|---|---|
| `-A`, `--allow-all` | allow everything (trimmed by any `--deny-*`) |
| `-R` `-W` `-N` `-E` `-S` | allow read/write/net/env/sys (whole category); `-R=./data` etc. scopes it |
| `--allow-<cat>[=list]` | grant a category (no value = whole category) |
| `--deny-<cat>[=list]` | deny within an allowed category |
| `-P`, `-P=<name>`, `--permission-set[=<name>]` | apply a named permission set from the config (bare `-P` = the `default` set) |
| `--runtime <ver>` | use a specific installed runtime tuple instead of the newest |

Examples:

```sh
inka run app.ts                          # deny-by-default
inka run -A app.ts                       # allow everything
inka run -R=./data app.ts                # read ./data only
inka run --allow-net=api.example.com app.ts
inka run -P server app.ts                # the config's `server` set
inka run -A --deny-read=./secrets app.ts # allow-all, minus secrets
inka run -- app-with-dashes.js           # end options; file may start with '-'
```

Rules:

- `-A` cannot combine with `-P` or `--allow-*` (deny-* may trim it); `-P`
  cannot combine with granular flags.
- Only `-P` is honored for `run` — `compile.permissions` and auto-defaults are
  never applied (that's a *build* concept).
- Repeated allow/deny flags merge per category (`*` wins, lists join).
- If you run with no permission flags and your config declares a non-empty
  `permissions.default`, inka prints a hint reminding you the run is
  deny-by-default.

## Execution root

The entry resolves like a build: a file under the current directory is rooted at
the current directory; an **outside-cwd file** is rooted at its nearest ancestor
project (a `package.json` or `deno.json`) or its own directory otherwise.
Relative imports stay bounded by that root.

## See also

- [Permissions](permissions.md) — the model behind the flags
- [`inka build`](build.md) — baking permissions into an artifact
- [Troubleshooting](troubleshooting.md) — exit codes and errors

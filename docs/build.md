# `inka build`

Packs a source file (and the files it imports) onto the launcher into a single
executable.

```sh
inka build [source] [-s|--source <file>] [-o|--output <file>]
           [--runtime <spec>] [--tested-against <ver>]
           [-P <name>] [--transpile] [--embed-dir] [--vendor-closure] [--no-vendor]
```

## Defaults

- **Source** — the positional argument, or `-s/--source`.
- **Output** — the source name without its extension (`app.ts` → `./app`), or
  `-o <file>`. `-o` must not overwrite the source.
- **Launcher** — found at `$INKA_LAUNCHER`, else `inka-launcher` next to the
  `inka` binary.

## The embedded manifest

`inka build` derives a small `key=value` manifest from your project config and
embeds it in the executable. It tells the runtime what the artifact needs and
may do:

```
runtime=inka_runtime>=0.266.3     # minimum engine floor
tested-against=0.266.3            # optional cap: never auto-run on something newer
module=app.js                     # entry name (always derived from the build)
allow-read=./data,/etc            # permissions
```

There is **no on-disk manifest input**: `module=` is always the packed entry,
and the runtime requirement comes from `inka.runtime` (or `--runtime`) with a
default floor of `>=0.266.3`. The floor is always embedded, so an artifact can
never select a runtime too old to enforce its permissions.

## Permissions from project config

Permission lines are baked **only from explicit build-intent sources**, never
from a bare dev-run `permissions.default` (matching Deno's model — a script
could modify `deno.json` to elevate permissions):

| Source | Result |
|---|---|
| `inka build -P <set>` | the named set from `deno.json.permissions.<set>` then `package.json.permissions.<set>` (deno wins per key when both define it) |
| `deno.json.compile.permissions` (category map, or a string naming a set) | baked automatically (build *is* the compile step) |
| `inka.permissions = "<set>"` marker (under the `inka` block; deno wins) | that named set |
| `permissions.default.<cat>` with **no** marker | **ignored** + a warning; artifact stays deny-by-default |
| *(none)* | deny-all + `runtime=inka_runtime>=0.266.3` |

A plain `permissions.default` set exists so local runs (`deno run -P`,
`deno task`) are frictionless; to bake it explicitly use `-P default`,
`compile.permissions: "default"`, or an `inka.permissions` marker. Unknown or
malformed sources warn and produce a deny-by-default artifact — never a silent
fall-back. `inka.runtime` / `inka.tested-against` emit `runtime=…` /
`tested-against=…`; `--runtime '<spec>'` / `--tested-against <ver>` override the
config.

Full details, including the config-set shapes: [Permissions](permissions.md).

## Local imports & multi-file apps

Importing other files just works — build from the project root so the entry has
a cwd-relative path:

```sh
# src/main.ts importing ./lib/util.ts etc.
inka build src/main.ts     # -> ./src/main executable
./src/main
```

Embedding is automatic and happens in one of two modes:

- **Import closure (default):** static imports/exports, literal `import("./x")`,
  and `.json` are discovered from the entry and embedded. A non-literal dynamic
  `import(...)` can't be seen statically → a warning suggests `--embed-dir`.
- **`--embed-dir`:** embed the whole current-directory tree (skipping `.git`,
  `target`, `node_modules`, `.inka`, `dist`) for computed dynamic imports.

### Vendored-package embedding

By default the whole per-project `vendored/` pool is embedded so artifacts are
self-contained. Two flags tune that for import-closure builds (combining either
with `--embed-dir` errors):

- `--vendor-closure` — embed only the vendored modules reachable from the entry
  graph (each reached root's `package.json` included). Store-only packages still
  resolve from the machine store at run time.
- `--no-vendor` — embed no vendored packages; the artifact relies on the machine
  default store (clean store-lookup error if a package is only vendored).

## TypeScript

Single-file and multi-file TypeScript work with no extra steps:

```sh
inka build app.ts        # -> ./app (module=app.ts is set for you)
./app kook
```

TS → JS happens one of two ways:

- **Runtime transpile (default):** the artifact keeps your `.ts` source; the
  shared runtime transpiles it at load with its own compiler.
- **Build-time transpile:** `inka build app.ts --transpile` ships pure JS.
  Multi-file apps work too — each `.ts/.mts/.cts` module is transpiled at build
  time but keeps its original archive path (no import rewriting), and the
  archive trailer tells the runtime not to re-transpile.

`.tsx`/`.jsx` are not supported yet (`--transpile` handles `.ts/.mts/.cts`;
JSX entries error).

## See also

- [`inka run`](run.md) — the no-build dev runner
- [Permissions](permissions.md)
- [Packages & the store](packages.md)
- [CLI reference](cli.md)

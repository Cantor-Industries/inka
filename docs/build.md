# `inka build`

Bundles a source file and its dependencies into one self-contained module, then
packs it onto the launcher into a single executable.

```sh
inka build [source] [-s|--source <file>] [-o|--output <file>]
           [--runtime <spec>] [--tested-against <ver>] [-P <name>]
           [--minify] [--sourcemap] [--external <pkg>]... [--embed-dir]
```

## Defaults

- **Source** — the positional argument, or `-s/--source`.
- **Output** — the source name without its extension (`app.ts` → `./app`), or
  `-o <file>`. `-o` must not overwrite the source.
- **Launcher** — found at `$INKA_LAUNCHER`, else `inka-launcher` next to the
  `inka` binary.

## What gets bundled

`inka build` resolves the entry's imports (import maps, bare `node_modules`,
`npm:`, `jsr:`, `node:`) and bundles them with
[rolldown](https://rolldown.rs) into one ESM module. TypeScript is transpiled and
tree-shaking is on. `node:` built-ins stay external (the engine provides them).

- `--minify` — minify the bundle.
- `--sourcemap` — embed an inline source map.
- `--external <pkg>` — leave a package **unbundled** but embed its files from
  `node_modules` (repeatable). Use it for native `.node` addons and packages that
  cannot be statically bundled.
- `--embed-dir` — also embed the whole current-directory tree (minus `.git`,
  `target`, `node_modules`, `.inka`, `dist`) for arbitrary asset files.

The result is an `INKFOOT5` artifact: a bundle plus any embedded files, so it
needs no `node_modules` or Deno cache at run time.

## The embedded manifest

`inka build` derives a small `key=value` manifest from your project config and
embeds it. It tells the runtime what the artifact needs and may do:

```
runtime=inka_runtime>=0.266.2     # minimum engine floor
tested-against=0.266.2            # optional cap: never auto-run on something newer
module=main.js                    # entry name (always derived from the build)
allow-read=./data,/etc            # permissions
```

There is **no on-disk manifest input**: `module=` is always the packed entry, and
the runtime requirement comes from `inka.runtime` (or `--runtime`) with a default
floor of `>=0.266.2`. The floor is always embedded, so an artifact can never
select a runtime too old to enforce its permissions.

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
| *(none)* | deny-all + `runtime=inka_runtime>=0.266.2` |

Full details, including the config-set shapes: [Permissions](permissions.md).

## TypeScript

TypeScript works with no extra steps — the bundler transpiles it:

```sh
inka build app.ts        # -> ./app
./app kook
```

`.tsx`/`.jsx` are supported by the bundler.

## See also

- [`inka run`](run.md) — the no-build dev runner
- [Permissions](permissions.md)
- [Dependencies & resolution](packages.md)
- [CLI reference](cli.md)

# `inka build`

Bundles a source file and its dependencies into one self-contained module, then
packs it onto the launcher into a single executable.

```sh
inka build [source] [-s|--source <file>] [-o|--output <file>]
           [--runtime <spec>] [--tested-against <ver>]
           [-A|--allow-all] [-R|-W|-N|-E|-S[=list]]
           [--allow-<cat>[=list]] [--deny-<cat>[=list]] [-P[=<set>]]
           [--minify] [--sourcemap] [--external[=<pkg>]]... [--embed-dir]
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
- `--external <pkg>` (or `--external=<pkg>`) — leave a package **unbundled** but
  embed its files from `node_modules` (repeatable). Use it for native `.node`
  addons and packages that cannot be statically bundled. The package's
  transitive dependency closure is embedded as well (hoisted and symlinked
  isolated layouts included).
- `--embed-dir` — also embed the whole current-directory tree (minus `.git`,
  `target`, `node_modules`, `.inka`, `dist`) for arbitrary asset files. Every
  dot-prefixed file/dir is skipped, and the ignore names apply at any depth.

`npm:` version pins are **enforced**: if the version in `node_modules` does not
satisfy an `npm:pkg@<req>` specifier, the build fails (mirroring `inka run`).

CommonJS dependencies that use the Node ambient `__filename`/`__dirname` are
shimmed to `import.meta.filename`/`import.meta.dirname` (the bundle's path): a
single-file ESM artifact has no per-module filenames, and the wrapper rolldown
generates only supplies `exports`/`module`.

The result is an `INKFOOT5` artifact: a bundle plus any embedded files, so it
needs no `node_modules` or Deno cache at run time.

## The embedded manifest

`inka build` derives a small `key=value` manifest from your project config and
embeds it. It tells the runtime what the artifact needs and may do:

```
# runtime floor; also >, ==, or a bare exact version
runtime=inka_runtime>=0.266.3
# optional cap: never auto-run on something newer
tested-against=0.266.3
# entry name (always derived from the build)
module=main.js
# permissions
allow-read=./data,/etc
```

The manifest is line-oriented; a `#` comment must be on its own line (trailing
text after a value is not stripped).

There is **no on-disk manifest input**: `module=` is always the packed entry, and
the runtime requirement comes from `inka.runtime` (or `--runtime`) with a default
floor of `>=0.266.3`. The floor is always embedded, so an artifact can never
select a runtime too old to enforce its permissions.

## Permissions from project config

Permission lines are baked from **explicit build-intent sources**. The CLI
grant flags mirror [`inka run`](run.md) and **override** any config-derived
permissions:

| CLI flag | Result |
|---|---|
| `-A`, `--allow-all` | `permissions=all` (trimmed by any `--deny-*`) |
| `-R`, `-W`, `-N`, `-E`, `-S[=list]` | grant read/write/net/env/sys (whole category, or scoped) |
| `--allow-<cat>[=list]` | grant `read\|write\|net\|env\|run\|sys\|ffi\|import` |
| `--deny-<cat>[=list]` | deny within an allowed category; **requires** an allow source (`-A` or `--allow-*`) |
| `-P[=<set>]`, `--permission-set <set>` | a named set from config (bare `-P` = `default`) |

A `--deny-*` with no allow source is an error (it would otherwise be silently
ignored). The same applies to `inka run`.

Without CLI flags, the build-intent source is chosen in this order:

| Source | Result |
|---|---|
| `deno.json.compile.permissions` (category map, or a string naming a set) | baked automatically (build *is* the compile step) |
| `inka.permissions` marker (`deno.json` wins over `package.json`): `"all"`, a set-name string, or a category map | baked automatically |
| `permissions.default.<cat>` with **no** marker | **ignored** + a warning; artifact stays deny-by-default |
| *(none)* | deny-all + a warning pointing at the grant options |

The `inka.permissions` marker is the **manager-agnostic** path — it works in
`package.json` for npm/pnpm/yarn/bun projects with no `deno.json`:

```jsonc
// package.json
{
  "inka": { "permissions": { "env": true, "read": ["./data"] } }
}
```

```jsonc
// or bake allow-all / a named set
{ "inka": { "permissions": "all" } }
{ "permissions": { "server": { "net": true } }, "inka": { "permissions": "server" } }
```

CLI flags always win, so `inka build -A app.ts` needs no config at all.
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

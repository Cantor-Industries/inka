# Permissions

inka permissions are **deny-by-default**: an artifact grants nothing unless its
manifest says so, and prompting is disabled. You grant access at **build time**
(for shipped artifacts, baked into the file) or at **run time** (with `inka run`
flags). Descriptor syntax and enforcement match Deno exactly — violations
surface as `NotCapable`/`PermissionDenied`, and `Deno.permissions` works.

Categories: `read`, `write`, `net`, `env`, `run`, `sys`, `ffi`, `import`.
`import` grants Deno's import permission for **cached** remote/`jsr:` modules
(inka still never fetches from the network). Deno's `ignore` sub-key has no inka
equivalent (warned and skipped).

Native `.node` (N-API) addons are `dlopen`ed at run time, so they need `ffi`
(scoped to the addon path). Addons that probe the platform also need `sys`
(e.g. `detect-libc`). Deny-by-default means an addon load without `ffi` fails
with `NotCapable` — see [Dependencies & resolution](packages.md#native-addons-node).

## The permission DSL

The artifact manifest (and the flags passed to `inka run`) use this DSL:

```
permissions=all                    # allow everything, trimmed by any deny-* below
allow-read=/etc,./data             # grant just this category (others denied)
deny-read=./data/secret.txt        # deny overrides allow / trims permissions=all
```

| Lines | Effective permissions |
|---|---|
| *(none)* or `permissions=none` | **deny everything** |
| `permissions=all` | allow everything (trimmed by `deny-*`) |
| `allow-<cat>=…` | grant that category only; unmentioned categories denied |
| `deny-<cat>=…` | trims an allowed category; at the DSL level a deny with no allow is a no-op that warns (the `inka` CLI rejects `--deny-*` with no allow source) |
| `*` in a list | all of that category |

Lists are comma-separated; a `deny-*` entry overrides an allow for the same
resource. An empty `allow-<cat>=` list is malformed and rejected — write
`allow-<cat>=*` to allow the whole category.

## Where permissions come from (build)

Permission lines baked by `inka build` come from **explicit build-intent
sources**. The CLI grant flags (`-A`/`--allow-*`/`-P`) take precedence and
override any config-derived permissions:

1. **CLI flags** — `-A`/`--allow-all`, `-R`/`-W`/`-N`/`-E`/`-S[=list]`,
   `--allow-<cat>[=list]`, `--deny-<cat>[=list]`, or `-P=<set>` /
   `--permission-set <set>` (bare `-P` = the `default` set). This is the only
   path that needs no config at all.
2. `deno.json.compile.permissions` — the deno-compile analog: a category map, or
   a string naming a set. Baked automatically because building *is* the compile
   step (a deliberate divergence from Deno, which needs an explicit `-P`).
3. An `inka.permissions` marker under the `inka` block (`deno.json` wins if both
   files have one). It accepts three shapes, so **projects without a
   `deno.json`** (npm/pnpm/yarn/bun `package.json`) can use it too:
   - `"all"` → `permissions=all`
   - a set-name string → that named set from `permissions.<name>`
   - a category-map object → an inline grant (`{ "env": true, "read": ["./"] }`)
4. A plain `permissions.default.<cat>` with **no** marker above is **ignored** —
   it's dev-run intent (`deno run -P`, `deno task`); builds warn and stay
   deny-by-default. Select it explicitly to bake it (`-P=default`,
   `compile.permissions: "default"`, or the marker).

With no source at all, `inka build` warns and produces a deny-by-default
artifact, pointing at the grant options.

Unknown or malformed sources (e.g. `-P=nope`, a bad `compile.permissions`, a
non-string/object `inka.permissions`) warn and produce a deny-by-default
artifact — never a silent fall-back.

## Config set shapes

Sets live in `deno.json` (and `package.json`) in the Deno shape:

```jsonc
{
  "permissions": {
    "default": { "env": true },                     // bool/array/scalar-string/object
    "server": {
      "read": ["./data"],
      "net": { "allow": ["0.0.0.0:80"], "deny": ["10.0.0.1"] }
    }
  },
  "compile": { "permissions": { "read": "./data" } } // or "permissions": "server"
}
```

See [`inka build`](build.md) for the full precedence table and edge cases.

> `package.json` is not a Deno config file, so its top-level `permissions` key is
> an inka extension. If some other tool also uses a `permissions` key there,
> inka will interpret it as Deno permission sets. Only explicit build-intent
> sources (`-P`, `compile.permissions`, an `inka.permissions` marker) actually
> bake, so the collision is inert unless one of those selects the set.

## Relative paths resolve at run time

`allow-read`/`allow-write` entries are passed through verbatim and interpreted
at **run time against the directory the executable is launched from** (Deno
semantics) — not the project dir at build time. Copying an exe to another
directory changes which paths a relative grant covers. This is convenient in
dev but easy to misread for a portable artifact. To pin grants to the build
layout, write **absolute paths** in your config; the trade-off is that absolute
paths tie the artifact to a machine's path layout. `inka build` warns when a
**config-sourced** set bakes a relative `read`/`write` grant; grants passed
directly on the CLI (`--allow-read=./x`) are not checked.

A permission item containing a raw newline (or a comma inside an array item) is
rejected at build time, as is an invalid `inka.runtime`/`--runtime` version
spec — malformed values never silently weaken the manifest.

## Hardening notes

- **Fail-closed:** if an artifact declares permissions but the installed runtime
  predates the `_perm` ABI, launching errors (exit 4) rather than silently
  running allow-all.
- **Policy is a property of the runtime build:** runtimes built before the
  deny-by-default change ran permission-less artifacts allow-all; current builds
  deny. Rebuild old artifacts against the current runtime to inherit the new
  default (or declare `permissions=all`).
- `inka run` grants are per-invocation flags (see [`inka run`](run.md)); `run`
  honors `-P` only and never applies `compile.permissions`/auto-defaults. When
  the config defines a build-intent source, `run` prints a hint naming the
  `-P=<set>` that reproduces the build's permissions.

## See also

- [`inka build`](build.md) · [`inka run`](run.md) · [CLI reference](cli.md)
- [Troubleshooting](troubleshooting.md) — `NotCapable` and denial errors

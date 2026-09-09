# Permissions

inka permissions are **deny-by-default**: an artifact grants nothing unless its
manifest says so, and prompting is disabled. You grant access at **build time**
(for shipped artifacts, baked into the file) or at **run time** (with `inka run`
flags). Descriptor syntax and enforcement match Deno exactly — violations
surface as `NotCapable`/`PermissionDenied`, and `Deno.permissions` works.

Categories: `read`, `write`, `net`, `env`, `run`, `sys`, `ffi`. Deno's
`import` category and the `ignore` sub-key have no inka equivalent (warned and
skipped).

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
| `deny-<cat>=…` | trims an allowed category; otherwise a no-op that warns |
| `*` in a list | all of that category |

Lists are comma-separated; a `deny-*` entry overrides an allow for the same
resource.

## Where permissions come from (build)

Permission lines baked by `inka build` come **only from explicit build-intent
sources**, in precedence order (matching Deno — config permissions are never
trusted implicitly, since a script could modify `deno.json` to elevate them):

1. `inka build -P <set>` — a named set from `deno.json.permissions.<set>` then
   `package.json.permissions.<set>` (deno.json wins per key when both define it).
2. `deno.json.compile.permissions` — the deno-compile analog: a category map, or
   a string naming a set. Baked automatically because building *is* the compile
   step (a deliberate divergence from Deno, which needs an explicit `-P`).
3. An `inka.permissions = "<set>"` marker under the `inka` block (deno wins if
   both files have one).
4. A plain `permissions.default.<cat>` with **no** marker above is **ignored** —
   it's dev-run intent (`deno run -P`, `deno task`); builds warn and stay
   deny-by-default. Select it explicitly to bake it (`-P default`,
   `compile.permissions: "default"`, or the marker).

Unknown or malformed sources (e.g. `-P nope`, a bad `compile.permissions`)
warn and produce a deny-by-default artifact — never a silent fall-back.

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

## Relative paths resolve at run time

`allow-read`/`allow-write` entries are passed through verbatim and interpreted
at **run time against the directory the executable is launched from** (Deno
semantics) — not the project dir at build time. Copying an exe to another
directory changes which paths a relative grant covers. This is convenient in
dev but easy to misread for a portable artifact. To pin grants to the build
layout, write **absolute paths** in your config or `.manifest`; the trade-off is
that absolute paths tie the artifact to a machine's path layout.

## Hardening notes

- **Fail-closed:** if an artifact declares permissions but the installed runtime
  predates the `_perm` ABI, launching errors (exit 4) rather than silently
  running allow-all.
- **Policy is a property of the runtime build:** runtimes built before the
  deny-by-default change ran permission-less artifacts allow-all; current builds
  deny. Rebuild old artifacts against the current runtime to inherit the new
  default (or declare `permissions=all`).
- `inka run` grants are per-invocation flags (see [`inka run`](run.md)); `run`
  honors `-P` only and never applies `compile.permissions`/auto-defaults.

## See also

- [`inka build`](build.md) · [`inka run`](run.md) · [CLI reference](cli.md)
- [Troubleshooting](troubleshooting.md) — `NotCapable` and denial errors

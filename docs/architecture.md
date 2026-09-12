# Architecture

An inka executable is a **small launcher** + your payload + a **manifest**. The
JS engine is not in the file: it is a shared runtime tuple installed once per
machine and loaded at run time.

## Pieces

| Piece | What it is | Where |
|---|---|---|
| Launcher | small Rust program (`crates/inka-launcher`) | copied into every artifact |
| Manifest | a few text lines | appended inside the artifact |
| Runtime | `libinka_runtime-<v>.so` — the Deno engine (`crates/inka-runtime`) | per-user `~/.local/share/inka/runtime` (`INKA_RUNTIME_HOME`) |

The runtime tuple's version is the pinned `deno_runtime` base (`0.xxx.0`) plus an
inka runtime revision (`.1`, `.2`, …), tracked in
`crates/inka-runtime/runtime-version`. Engine-only fixes advance the revision so
they are delivered as a new tuple without changing the `deno_runtime` pin.

## The artifact layout

`inka build` concatenates, in order:

```
[ launcher bytes ]
[ payload: one source file, or a files archive ]
[ manifest bytes ]
[ footer: magic + payload length + manifest length ]
```

The footer is the last 24 bytes: an 8-byte magic plus two little-endian `u64`
lengths. Magic values:

- `INKFOOT2` — a single embedded source file
- `INKFOOT3` — a multi-file archive
- `INKFOOT4` — a multi-file archive with TypeScript pre-transpiled to JavaScript

At run time the launcher reads its own executable, parses the trailer, and
extracts the archive to a temp tree when needed.

## The manifest

Recognized keys:

```
runtime=inka_runtime>=0.266.4     # floor (also >, ==, or bare exact)
tested-against=0.266.4            # cap: do not roll forward past this
module=main.js                    # entry file inside the payload
permissions=all                   # or allow-<cat>=… / deny-<cat>=…
allow-read=./data,/etc
```

Permissions are forwarded verbatim to the runtime. Artifacts are
deny-by-default unless an allow source was baked at build time (see
[Permissions](permissions.md)).

## Runtime selection

The launcher searches `$INKA_RUNTIME_HOME` and `~/.local/share/inka/runtime`,
then picks the **newest** tuple that satisfies the manifest's `runtime=` floor
and `tested-against=` cap. `$INKA_RUNTIME` forces a specific file. No match →
exit 3. Updating the machine's runtime updates every artifact at once.

## C ABI

The launcher `dlopen`s the runtime and calls a frozen C ABI:

- `inka_runtime_version() -> const char*`
- `inka_runtime_create() -> *mut void`
- `inka_runtime_run_module_perm(rt, name, payload, len, argc, argv, out_exit, out_err, perms)`
- `inka_runtime_run_module_dir(rt, dir, entry, argc, argv, out_exit, out_err, perms)`
- `inka_runtime_destroy(rt)`

The runtime is loaded **`RTLD_GLOBAL`** (not the `RTLD_LOCAL` default) so native
`.node` addons `dlopen`ed later by the runtime can resolve the N-API/uv symbols
it exports; with `RTLD_LOCAL` the addon aborts with `undefined symbol:
napi_module_register`. See [Packages & the store](packages.md#commonjs).

`_perm` and `_dir` are mandatory: a runtime that lacks either is refused
(exit 4) rather than run with the wrong semantics. There is no permission-less
entry point, so deny-by-default cannot degrade to allow-all.

## Resolution

ESM `import` and CJS `require()` share one policy: Deno's store-backed
`NodeResolver`, driven by a nearest-`node_modules` walk over three derived
roots in precedence order — `vendored/node_modules`, the project/artifact
`node_modules` (BYONM), then the default store — followed by built-ins. Nested
packages beat hoisted ones; store referrers stay confined to the store. See
[Packages & the store](packages.md).

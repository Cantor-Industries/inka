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
[ payload: a bundle plus optional embedded files, or a legacy archive/source ]
[ manifest bytes ]
[ footer: magic + payload length + manifest length ]
```

The footer is the last 24 bytes: an 8-byte magic plus two little-endian `u64`
lengths. The only magic is `INKFOOT5` — a bundle plus optional embedded files;
`inka build` emits it and the launcher accepts nothing else.

At run time the launcher reads its own executable, parses the trailer, and
extracts the archive to a temp tree when needed.

## The manifest

Recognized keys (line-oriented; `#` comments must be on their own line):

```
# floor (also >, ==, or bare exact)
runtime=inka_runtime>=0.266.5
# cap: do not roll forward past this
tested-against=0.266.6
# entry file inside the payload
module=main.js
# or allow-<cat>=… / deny-<cat>=…
permissions=all
allow-read=${EXE_DIR}/data,/etc
# engine capabilities the artifact needs (verified by the launcher)
requires=raw-cjs
# anchor relative read/write grants (exe|cwd)
path-base=exe
```

The default floor is the **security floor** (the tuple that enforces
deny-by-default, realpath confinement, and the `_dir`-only entry); `requires=`
lists capabilities the bundle actually uses, and the launcher verifies them
against the runtime's `inka_runtime_features()` when it is available. This lets a
simple artifact run on an older installed tuple while a capability-dependent one
is rejected with a precise message.

Permissions are forwarded verbatim to the runtime (after host-side token
expansion). Artifacts are deny-by-default unless an allow source was baked at
build time (see [Permissions](permissions.md)).

## Runtime selection

The launcher searches `$INKA_RUNTIME_HOME` and `~/.local/share/inka/runtime`,
then picks the **newest** tuple that satisfies the manifest's `runtime=` floor
and `tested-against=` cap. The runtime's self-reported `inka_runtime_version()`
must match the version in its filename, or the launcher refuses to load it. No
match → exit 3. Updating the machine's runtime updates every artifact at once.

## C ABI

The launcher `dlopen`s the runtime and calls a frozen C ABI:

- `inka_runtime_version() -> const char*`
- `inka_runtime_create() -> *mut void`
- `inka_runtime_run_module_dir(rt, dir, entry, argc, argv, out_exit, out_err, perms)`
- `inka_runtime_destroy(rt)`
- `inka_runtime_free_string(ptr)` — optional; frees an error string the runtime
  allocated. A runtime without it is still usable (the string leaks until exit).
- `inka_runtime_features() -> const char*` — optional; a comma-separated list of
  engine capabilities (`raw-cjs`, `native-addon`, `import-perm`, `tsconfig-run`,
  `workspace`, …). The launcher verifies a manifest's `requires=` against it when
  present, and falls back to the version floor otherwise.

The runtime is loaded **`RTLD_GLOBAL`** (not the `RTLD_LOCAL` default) so native
`.node` addons `dlopen`ed later by the runtime can resolve the N-API/uv symbols
it exports; with `RTLD_LOCAL` the addon aborts with `undefined symbol:
napi_module_register`. See [Dependencies & resolution](packages.md#native-addons-node).

`_dir` is mandatory: a runtime that lacks it is refused (exit 4) rather than
run with the wrong semantics. There is no permission-less entry point, so
deny-by-default cannot degrade to allow-all.

The ABI is **additive**: a release may add an optional symbol (the launcher and
`inka run` probe it and fall back when absent), but removing or renaming a
required symbol is a breaking tuple event. `scripts/ci/abi-symbols.sh` gates the
exported `inka_runtime_*` set in CI.

Portable permission tokens (`${EXE_DIR}`/`${PROJECT_DIR}`) and `path-base=exe`
are expanded **host-side** (launcher / `inka run`) before the DSL reaches the
runtime, so the engine still sees ordinary Deno descriptors.

## Resolution

Resolution is **offline**. ESM `import` and CJS `require()` share one policy:
a `deno.json` import map, then a nearest-`node_modules` walk over the
project/artifact tree (BYONM; nested beats hoisted), then `jsr:`/remote modules
from the Deno cache (`$DENO_DIR`), then `node:` built-ins. `npm:` packages
resolve from `node_modules`. `inka build` bundles this graph up front; `inka run`
resolves it live. Module reads are confined to the execution tree's realpath;
an out-of-tree package is served only when an explicit read grant covers it.
See [Dependencies & resolution](packages.md).

# Architecture

An inka executable is a **small launcher** + your payload + a **manifest**. The
JS engine is not in the file: it is a shared runtime tuple installed once per
machine and loaded at run time.

## Pieces

| Piece | What it is | Where |
|---|---|---|
| Launcher | small Rust program (`crates/inka-launcher`) | copied into every artifact |
| Manifest | a few text lines | appended inside the artifact |
| Runtime | `libinka_runtime-<v>.so` — the Deno engine (`crates/inka-runtime`) | system `/usr/local/lib/inka-runtime` or per-user `~/.local/share/inka/runtime` |
| Resolver | `libinka_resolver-<v>.so` — package resolution | alongside the runtime |

The runtime tuple's version is the pinned `deno_runtime` crate version.

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
runtime=inka_runtime>=0.266.0     # floor (also >, ==, or bare exact)
tested-against=0.266.0            # cap: do not roll forward past this
module=main.js                    # entry file inside the payload
permissions=all                   # or allow-<cat>=… / deny-<cat>=…
allow-read=./data,/etc
```

Permissions are forwarded verbatim to the runtime. Artifacts are
deny-by-default unless an allow source was baked at build time (see
[Permissions](permissions.md)).

## Runtime selection

The launcher searches `$INKA_RUNTIME_HOME`, `~/.local/share/inka/runtime`, and
`/usr/local/lib/inka-runtime`, then picks the **newest** tuple that satisfies the
manifest's `runtime=` floor and `tested-against=` cap. `$INKA_RUNTIME` forces a
specific file. No match → exit 3. Updating the machine's runtime updates every
artifact at once.

## C ABI

The launcher `dlopen`s the runtime and calls a frozen C ABI:

- `inka_runtime_version() -> const char*`
- `inka_runtime_create() -> *mut void`
- `inka_runtime_run_module[_perm](rt, name, payload, len, argc, argv, out_exit, out_err, perms)`
- `inka_runtime_run_module_dir(rt, dir, entry, argc, argv, out_exit, out_err, perms)`
- `inka_runtime_destroy(rt)`

A runtime that predates `_perm`/`_dir` is refused (exit 4) rather than run with
the wrong semantics.

## Resolution

Bare/`npm:`/`jsr:` imports resolve through the resolver ABI (`inka_resolver_abi`
must equal 2) against the project `vendored/` root, then the default store, then
built-ins. See [Packages & the store](packages.md).

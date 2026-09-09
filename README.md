# inka

Tiny single-file executables for JavaScript/TypeScript that run on a **shared, tuple-versioned Deno runtime** instead of bundling one into every binary.

## How it works: the launcher, the manifest, and the embed model

Normal "compiled" JS binaries (`deno compile`, `bun build --compile`) work by **fusing the JS engine into the file**. Result: a ~100 MB "Hello World". The engine (V8 + snapshot + ICU) is huge, and every artifact carries its own copy.

The inka idea: **don't put the engine in the file.** Put a *tiny host* in the file, and have the engine be one shared library installed once per machine that any inka executable can load.

So there are exactly three things in play:

| Piece | What it is | Where it lives | Size |
|---|---|---|---|
| **Launcher** | A small native program compiled from Rust (`crates/inka-launcher`) | **copied inside every artifact** | ~355 KB |
| **Manifest** | A few lines of text describing the artifact | appended inside the artifact | ~50 B |
| **Runtime** | `libinka_runtime-<version>.so` — the real Deno engine (from `crates/inka-runtime`) | installed *once* per machine | ~96 MB |

### The artifact file is just a concatenation

`inka build app.js` glues bytes together and writes one file. Inside the artifact the bytes are, in order:

```
[ launcher executable bytes ]   ← the compiled Rust launcher
[ your program bytes        ]  ← a single source file, or a files archive
                                 when the app imports other files
[ your manifest bytes        ]  ← what it needs and may do
[ footer: "INKFOOT2"/"INKFOOT3" + length of program + length of manifest ]
```

The footer is the last 24 bytes: an 8-byte magic string (`INKFOOT2` = one embedded source, `INKFOOT3` = a multi-file archive) plus two 8-byte lengths. That is the only "clever" part — it is how the launcher finds its data when it runs.

Why does this work? Because an executable doesn't have to be *only* machine code. The OS runs the code at the start of the file and ignores trailing garbage. Appending text to a compiled binary is safe. That is the entire "embed" model: **the source and manifest are just extra bytes stuck onto the end of the launcher.**

### What the launcher does when you run `./app`

It has no idea what your program does — it is a generic host. At startup it:

1. **Finds itself** via `/proc/self/exe` (the path of the running file) and reads its own bytes.
2. **Locates the trailer** — reads the last 24 bytes, checks the magic, gets the two lengths, and slices out the source and the manifest from the middle.
3. **Reads the manifest** to learn what runtime version it needs (`runtime=…`), what to call the entry file (`module=…`), whether to pin roll-forward (`tested-against=…`), and what permissions to grant (`permissions=…`, `allow-*`, `deny-*`).
4. **Searches for a runtime** on the machine (`$INKA_RUNTIME_HOME` → `~/.inka-runtime` → `/usr/local/lib/inka-runtime`), scans for `libinka_runtime-*.so` files, parses versions out of the filenames, and picks the newest one that satisfies the manifest (≥ floor, ≤ `tested-against`).
5. **`dlopen`s that `.so`** (like loading a plugin) and calls a fixed C function, handing it the embedded source, the command-line args, and the permissions string.
6. **Stays alive as the host process.** The runtime starts V8 (fast — it has a pre-baked snapshot), creates a Deno worker, stages the source, transpiles it if it is `.ts`, executes it, and runs the event loop until the program finishes.
7. **Returns the exit code.** stdin/stdout/stderr are inherited, so `console.log` and friends just work.

The launcher never executes your code. It is a courier: it carries the source to the engine and reports back the exit code. Your code is interpreted/JIT-compiled **at run time by the runtime**, not at build time (unless you pass `--transpile`).

### Why the manifest exists

The runtime is versioned and shared, but different artifacts may need different things from it. The manifest is the artifact's **declaration of intent**, read by the launcher at run time:

```
runtime=inka_runtime>=0.266.0     # I need at least this engine
tested-against=0.266.0            # optional: never auto-run on something newer
module=app.js                     # display name for my entry source
allow-read=/etc,./data            # grant these file reads (deny everything else)
```

Without a manifest the launcher wouldn't know which of the possibly-several installed `.so` files to load, or under what permissions.

### What this buys

- **Small files.** The artifact is ~355 KB because it only carries *transport* (launcher) + *cargo* (source + manifest). The ~96 MB engine is a per-machine shared library, loaded by whatever file you run.
- **Runtime upgrades don't touch your files.** Install `libinka_runtime-0.267.0.so` on the machine and every artifact whose manifest says `>= 0.266.0` automatically starts using it — same source, same launcher, new engine underneath. Your code was never compiled against an engine, so nothing needs rebuilding.
- **Policy travels with the file** (permissions are embedded at build time) while **the engine stays shared**.

**The core guarantee:** your app artifact is *never rebuilt or re-shipped* when the runtime updates. You install a newer runtime once per machine; every inka executable on that machine keeps running against it.

The mental flip versus what you're used to: *the file you distribute is not the program.* The program is your source text; the file is a self-describing envelope that picks an engine off the shelf at run time and hands it your code.

```
┌────────────── your artifact (single file, ~0.35 MB) ──────────────┐
│  Rust launcher: trailer parse → manifest → tuple resolve → dlopen  │
│  [app source + manifest ← appended, magic+length footer at EOF]   │
└──────────────────────────────────┬─────────────────────────────────┘
                        dlopen (libloading)
┌──────────────────────────────────▼─────────────────────────────────┐
│  libinka_runtime-0.266.0.so  (Rust cdylib: deno_runtime 0.266.0)  │
│  [V8 engine + embedded startup snapshot + ICU — build-locked]      │
└────────────────────────────────────────────────────────────────────┘
```

## Workspace layout

| Crate | Role |
|---|---|
| `crates/inka-launcher` | Thin native host: parses the appended trailer, resolves a tuple, `dlopen`s it, runs your module |
| `crates/inka-runtime-stub` | Tiny fake `.so` exporting the same C ABI — used to develop/test the launcher cheaply |
| `crates/inka-runtime` | Real runtime: `deno_runtime` behind the frozen C ABI, with a V8 startup snapshot embedded at build time |
| `crates/inka-resolver` | Import-resolution/policy engine as its own cdylib (`libinka_resolver-<v>.so`), pure Rust + serde with **no** deno/V8 dependency — loaded by the runtime via a stable C ABI, so resolver changes rebuild in seconds without touching the engine |
| `crates/inka` | Companion CLI: `inka build` (pack launcher + source + manifest into an artifact), `inka install <version>` (checksum-gated runtime distribution), `inka list`, and `inka pkg` (tar/seed/list the vendored-package store) |

## The frozen C ABI (identical in stub and real runtime)

```c
const char* inka_runtime_version(void);
void*       inka_runtime_create(void);
int  inka_runtime_run_module(void*, const char* specifier,
                            const char* source, size_t len,
                            int argc, char** argv,
                            int* exit_code, char** err_msg);
int  inka_runtime_run_module_perm(void*, const char* specifier,   /* additive */
                            const char* source, size_t len,
                            int argc, char** argv,
                            int* exit_code, char** err_msg,
                            const char* perms);
int  inka_runtime_run_module_dir(void*, const char* dir_path,     /* additive */
                            const char* entry, int argc, char** argv,
                            int* exit_code, char** err_msg,
                            const char* perms);
void inka_runtime_destroy(void*);
```

`inka_runtime_run_module_perm` is the additive, permission-aware single-source entry point. `inka_runtime_run_module_dir` runs a multi-file artifact: `dir_path` points at an extracted tree and `entry` is the entry module's path inside it, resolved with its relative imports (`.ts` transpiled per file). The legacy `inka_runtime_run_module` is kept for older runtimes and behaves the same as `_perm` with empty permissions (deny-by-default) on current runtime builds. If an artifact declares permissions but the installed runtime lacks the `_perm` symbol, or is multi-file while the runtime lacks the `_dir` symbol, the launcher **fails closed** (exit 4).

Everything else (Deno.\*, Web APIs, the event loop) lives inside the `.so` and is invisible to the ABI.

## Runtime versioning ("tuples")

One installed runtime file = one version: `libinka_runtime-<version>.so`. The version number is the version of the embedded `deno_runtime` crate (0.266.0 ↔ Deno 2.9.6), which pins a specific V8 + snapshot + ICU, so the *tuple* collapses to a single version number.

Search order: `$INKA_RUNTIME_HOME` → `~/.inka-runtime` → `/usr/local/lib/inka-runtime`. `INKA_RUNTIME=/path/to/lib.so` forces one specific file.

Manifest policy:

```
# key=value
runtime=inka_runtime>=0.266.0     # minimum floor
tested-against=0.266.0            # optional cap: never auto-use newer than tested
module=main.js                    # display specifier
```

- No cap → newest installed tuple ≥ floor wins (roll-forward).
- With cap → newest installed tuple ≤ cap that is ≥ floor.
- No match → clear error naming the requirement and search paths (exit 3).

## Permissions

Permissions are baked into the artifact at `inka build` via the manifest — the executable is always launched plain (`./app ...`), never with `--allow-*` flags, and prompting is disabled. Categories: `read`, `write`, `net`, `env`, `run`, `sys`, `ffi`.

**Deny by default:** an artifact grants nothing unless its manifest says so.

```
runtime=inka_runtime>=0.266.0
permissions=all                    # allow everything, trimmed by any deny-* below
allow-read=/etc,./data             # grant just this category (others denied)
deny-read=./data/secret.txt        # deny overrides allow / trims permissions=all
```

Policy:

| Manifest | Effective permissions |
|---|---|
| *(no permission keys)* | **deny everything** |
| `permissions=none` | deny everything |
| `permissions=all` | allow everything (trimmed by `deny-*`) |
| `allow-<cat>=…` | grant that category only; unmentioned categories denied |
| `deny-<cat>=…` | trims an allowed category (`allow-*` or `permissions=all`); otherwise a no-op that prints a warning |
| `*` in a list | all of that category |

Descriptor syntax and enforcement match Deno exactly (`Deno.permissions` works, violations surface as `NotCapable`/`PermissionDenied`). Deny entries override allow entries.

Two notes:

- **Fail-closed:** if an artifact declares permissions but the installed runtime predates the `_perm` ABI, launching errors (exit 4) rather than silently running allow-all.
- **Relative `allow-read`/`allow-write` entries are interpreted at run time against the directory the executable is launched from** (Deno semantics). The paths in your config (`permissions` sets / `compile.permissions`) or a `.manifest` are passed through verbatim — copying an exe to another directory changes which paths a relative grant covers. This is convenient for dev (`allow-read=./data` next to where you run) but easy to misread for a portable artifact. To pin grants to the build-time project layout instead, write **absolute paths** in your config or `.manifest` — the trade-off: absolute paths tie the artifact to a machine's path layout, while relative entries match Deno and follow the launch directory. (Canonicalizing config paths to the build dir at bake time is a noted future option.)
- **Policy is a property of the runtime build:** runtimes built before the deny-by-default change ran permission-less artifacts allow-all; current builds deny. Rebuild old artifacts against the current runtime to inherit the new default (or declare `permissions=all`).

## Quickstart

```sh
cargo build --release -p inka -p inka-launcher

# 1. write your program
cat > app.js <<'EOF'
console.log(`hello ${Deno.args[0] ?? "world"} from deno ${Deno.version.deno}`);
EOF

# 2. manifest — optional. A `.manifest` file (app.manifest / inka.manifest) is
#    honored when present; otherwise it is auto-generated from package.json /
#    deno.json explicit build-intent permissions (an `inka.runtime` block, or
#    `compile.permissions`) with a default floor of runtime>=0.266.0 and
#    deny-by-default permissions.
printf 'runtime=inka_runtime>=0.266.0\nmodule=app.js\n' > app.manifest   # optional

# 3. pack  (manifest auto-found as app.manifest; output auto-derived as "app")
./target/release/inka build app.js
# ...or be explicit:
./target/release/inka build app.js --manifest app.manifest -o myapp

# 4. run (needs a compatible runtime installed)
./myapp kook
```

`inka build` finds the launcher automatically: `$INKA_LAUNCHER`, else `inka-launcher` next to the `inka` binary (so build `-p inka-launcher` too and keep them together). Defaults: source = positional arg (or `-s/--source`), output = source name without its extension, manifest = `--manifest` → `<source-stem>.manifest` → `inka.manifest` → auto-generated from project config (`package.json` + `deno.json`/`deno.jsonc`; `deno.json` wins per-key). `module=` is always set to the entry.

### `inka run` (dev execution)

`inka run <file> [args…]` executes a `.ts`/`.js`/`.mjs`/`.cts` file directly through the installed runtime — no artifact build. Relative imports, vendored packages, the default store, and `node:` built-ins all resolve exactly as they would in a built artifact; `.ts` is transpiled at load. Options must precede the file; anything after it (or after `--`) is passed to the program as its arguments, and the program's exit code is propagated. The execution root is the current directory when the file is under it; an outside-cwd file is rooted at its nearest ancestor project (a `vendored/`, `package.json`, or `deno.json`) or its own directory otherwise.

Permissions mirror `deno run --no-prompt` (deny by default, no prompting):

```sh
inka run app.ts                       # deny-by-default
inka run -A app.ts                    # allow everything
inka run -R app.ts                    # deno short form: allow read (also -W/-N/-E/-S)
inka run -R=./data app.ts             # scoped short form
inka run -P server app.ts             # named permission set from the config
inka run -P app.ts                    # bare -P = the config `default` set
inka run --allow-read=./data --allow-net app.ts   # granular grants
inka run -A --deny-read=./secrets app.ts          # allow-all, trimmed
```

Granular categories: `read`, `write`, `net`, `env`, `run`, `sys`, `ffi` (`--allow-<cat>[=list]` / `--deny-<cat>[=list]`; no value = the whole category; repeated flags merge per category). `-A`/`--allow-all` cannot be combined with `-P` or `--allow-*` (but may be trimmed by `--deny-*`); `-P` cannot be combined with granular flags. `--runtime <ver>` picks a specific installed tuple instead of the newest. When run with no permission flags and the config declares a non-empty `permissions.default`, a hint reminds you that the run is deny-by-default (permissions are never auto-applied).

### Manifest vs project config

When no `.manifest` file exists, `inka build` synthesizes one. Permission lines
are baked **only from explicit build-intent sources**, in this precedence order
(matching Deno's model — config permissions are never trusted implicitly, since
a script could modify `deno.json` to elevate them):

| Source | Emits |
|---|---|
| `inka build -P <set>` | the named set, resolved from `deno.json.permissions.<set>` then `package.json.permissions.<set>` (deno.json wins per key when both define it) |
| `deno.json.compile.permissions` (a category map, or a string naming a set) | the deno-compile analog; baked automatically because `inka build` *is* the compile step |
| `inka.permissions = "<set>"` marker (under the `inka` block; deno.json wins) | that named set |
| `permissions.default.<cat>` (Deno shape) with **no** marker above | **ignored** — this is dev-run intent (`deno run -P`); builds warn and stay deny-by-default |
| *(none)* | deny-all + `runtime=inka_runtime>=0.266.0` |

A plain `permissions.default` set exists so local runs (`deno task`, `deno run
-P`) are frictionless; it must never silently become the permission policy of a
shipped binary. To bake it anyway, select it explicitly (`-P default`,
`compile.permissions: "default"`, or an `inka.permissions` marker). Note this
auto-baking of `compile.permissions` is a deliberate divergence from Deno,
which requires an explicit `-P` even for `compile`/`test`/`bench` permissions.

`inka.runtime` / `inka.tested-against` emit `runtime=…` / `tested-against=…`.
Deno's `ignore` sub-key and the `import` category have no inka equivalent
(warned + skipped); scalar-string values like `"read": "./data"` are allowed.
An unknown or malformed permission source (e.g. `-P nope`, a bad
`compile.permissions`) warns and produces a deny-by-default artifact — never a
silent fall-back. `--runtime '<spec>'` / `--tested-against <ver>` override
config on the command line.

### Local imports & multi-file apps

Files that import other files are supported — build from the project root so the entry has a cwd-relative path:

```sh
# src/ dir with main.ts importing "./lib/util.ts" etc.
inka build src/main.ts            # -> ./src/main executable
./src/main                       # runs with its imports embedded
```

Embedding is automatic. When imports exist, `inka build` walks the import graph and packs exactly the referenced files into the artifact (a files archive + a new trailer). Two modes:

- **Import closure (default):** static imports/exports, literal `import("./x.js")`, and `.json` are discovered from the entry (via `deno_ast`) and embedded, preserving the cwd-relative tree. A non-literal dynamic `import(...)` can't be seen statically → a warning suggests `--embed-dir`.
- **`--embed-dir`:** embed the whole current-directory tree (skipping `.git`, `target`, `node_modules`, `.inka`, `dist`) for projects that use computed dynamic imports.

Vendored packages are embedded whole-pool by default so a built artifact is self-contained. Two flags tune that (import-closure builds only; combining either with `--embed-dir` errors):

- **`--vendor-closure`:** embed only the vendored modules reachable from the entry's import graph (walking *through* vendored packages; each reached root's `package.json` is included). Packages the app never touches stay out of the artifact, shrinking it — a package only satisfied by the default store is still resolved from the store at run time.
- **`--no-vendor`:** skip vendored embedding entirely; the artifact relies on the machine default store (clean store-lookup error if a package is only vendored).

TS is transpiled per file at run time by the tuple by default (so extensionless `./math` → `math.ts` etc. resolve like Deno). With `--transpile`, modules are compiled at build time instead (single- and multi-file). Bundled-module reads are part of the program and don't count against the `read` permission; `Deno.readTextFileSync` and other file/network ops remain permission-gated.

### TypeScript

Single-file TypeScript entries are supported as-is — no extra steps:

```sh
# app.ts (types, interfaces, generics, top-level await all fine)
./target/release/inka build app.ts        # -> ./app  (module=app.ts is set for you)
./app kook
```

Two ways to compile TS → JS:

- **Runtime transpile (default):** the artifact keeps your `.ts` source; the shared runtime `.so` transpiles it at load using the tuple's own TS compiler (so TS semantics track the runtime, not your machine).
- **Build-time transpile:** `inka build app.ts --transpile` compiles to JS while packing, so the artifact ships pure JavaScript. Multi-file apps work too (`demo/main.ts` importing `./lib.ts` etc.): each `.ts/.mts/.cts` module is transpiled to JS at build time but keeps its original path in the archive (Deno-style, no import rewriting); the archive trailer (`INKFOOT4`) tells the runtime to serve those modules as plain JS without re-transpiling. Relative and extensionless imports keep resolving as usual, and literal/computed dynamic imports still work.

`inka build` normalizes the manifest for you: if it has no `module=` line, one matching the source is appended (`.ts` preserved for runtime transpile, `.js` after `--transpile`); an explicit `module=` that contradicts the packed code's language prints a warning.

Install a runtime on a machine (requires a `<file>.sha256` sidecar or `--sha256 <hex>`; add `--insecure` to skip):

```sh
inka install 0.266.0 --from https://your-registry.example/runtimes
inka list
```

## Vendored packages (offline `npm:`/`jsr:`)

Artifacts can import real registry packages by name and get the **local** copy — never the network. No prefix needed: the runtime resolves a bare specifier against the store, matching npm and jsr packages alike (`@std/assert` is jsr, and jsr is served via its npm-mirror identity):

```js
import { z } from "zod";
import { assertEquals } from "@std/assert";
```

Node built-ins also need no `node:` prefix — `vm` and `node:vm`, `process` and `node:process` are interchangeable:

```js
import vm from "vm";
import process from "process";
```

Resolution happens at run time against a **package store** — one shared, hoisted `node_modules` pool on the machine (a normal npm project layout) — not by installing anything at run time. `node:` built-ins keep working as before.

**Per-project vendoring (`inka add`/`remove`).** Packages the default store doesn't cover (or that a project wants to pin itself) are vendored into a project-relative `vendored/` folder of **name-keyed package roots** (no `node_modules`; jsr uses its npm-mirror identity), declared as a union in the project's `package.json` (`dependencies`) and `deno.json` (`imports`), pinned exactly in `vendored.lock`. `inka build` embeds those roots into the artifact; the launcher auto-detects them and resolution for **app/vendored code** is `vendored/<name>` → default store → builtins (so a vendored copy can override the store or a builtin), while **imports from inside default-store packages** keep today's store → builtins semantics and never consult a project's vendored set. Embedded vendored packages make the artifact portable (copy it anywhere; it re-extracts its own copy). `inka remove` un-vendors a package and prunes orphaned vendored deps; `inka vendor list|status|release|ignore` manages the set and the dev-gitignore vs release-commit posture.

**jsr packages use their npm-mirror identity.** `inka add jsr:@std/path` is installed and recorded under the name npm itself uses for jsr packages, `@jsr/std__path` — as the vendored dir `vendored/@jsr/std__path`, as the `package.json` dependency key, as the `deno.json` `imports` key, and in `vendored.lock`. A project whose `package.json` is later read by npm therefore needs an `.npmrc` so the `@jsr` scope resolves to jsr's registry (the same line inka writes into its own scratch installs):

```sh
# .npmrc
@jsr:registry=https://npm.jsr.io
```

`inka remove` accepts either spelling — `inka remove jsr:@std/path` and `inka remove @jsr/std__path` both target the same vendored entry and clean the same manifest keys.

Store layout:

```
<store>/                                  ~/.inka-runtime/store  (or $INKA_STORE)
  seed-manifest.json                      # installed top-levels + snapshot sha256
  node_modules/…                          # the whole resolved tree (shared/hoisted)
```

Because it is a single pool, dependency graphs behave like any Node project: compatible versions share one hoisted copy; incompatible ones nest under their dependents and the runtime resolves them by nearest-`node_modules` (so independent packages can use different versions of a shared dependency, and Effect-style libraries keep a single copy when their versions align).

`<store>` defaults to `$INKA_STORE`, else a `store/` directory next to the chosen runtime (e.g. `~/.inka-runtime/store`); the launcher sets it for you when it exists, so plain `./app` runs work with no env.

The store is **curated by a `seed-manifest.json`**, not hard-coded: `{ "seed": [ { "name", "version", "registry" } ] }` (registry defaults to `npm`; use `"jsr"` for jsr packages). inka ships a default one; replace it to curate your own set. Discovery: `--seed-manifest` → `$INKA_SEED_MANIFEST` → `./seed-manifest.json` → next to the inka binary. The shipped default currently seeds `zod`, `@std/assert` (jsr), and the Effect trio `effect` + `@effect/platform` + `@effect/platform-node` (peers like `@effect/rpc`/`sql`/`cluster` auto-included).

Distribution is a **whole-store snapshot**: `snapshot` npm-installs the seed set together (pre/postinstall already run there, before the tar is made), applies any repo-managed **patches** under `patches/<pkg>/<version>/patch.json` (converting CJS packages the ESM-only engine can't run into engine-viable pure ESM — see *Current limits*), then packages the resolved `node_modules` as `store.tar.gz`. Consumers only download → verify → replace `node_modules`; nothing installs or runs on the machine. jsr is served through jsr's npm-mirror identity (`jsr:@scope/name` → `@jsr/scope__name`), so one mechanism covers both registries.

Commands (`crates/inka`):

- `inka pkg snapshot [--seed-manifest <file>] [--out <dir>]` — network-only (release/dev). Resolves the whole seed set with a package manager and writes `store.tar.gz` + `.sha256` + a `seed-manifest.json` record.
- `inka pkg seed --from <dir-or-url>` — fetch → sha256-verify → atomically replace the store's `node_modules`.
- `inka pkg list` — show installed top-levels as `name@version`.
- `inka install <version> --from <release>` — when the release carries a `store/` payload, seeds it too, so curated packages arrive together with the runtime (zero extra step for end users).

Version policy: bare imports load the store's hoisted copy of a package. An explicit pinned/range import (`npm:effect@3.22.1`, `jsr:@std/assert@1.0.0`) must match that hoisted version, otherwise a clean error tells you to re-seed. Anything absent is a clean `run "inka pkg seed"` error. The engine never fetches modules: `http(s):` imports are rejected outright.

### CJS→ESM conversion (patches + patcher)

The engine is ESM-only, so when `inka add` vendors a package it cannot run as-is (genuine CommonJS leaves, or dual packages like `ws` whose import facade wraps CJS), it converts it to engine-viable pure ESM. That needs two pieces colocated with the `inka` binary (no repo tree required):

- **Curated patch specs** — `patches/<name>/<version>/patch.json` (the repo ships `ws`/`undici`/`mime`/`msgpackr`). Discovered at `$INKA_PATCHES` → `./patches` → `<dir of inka binary>/patches`. When a curated spec exists it is applied automatically; otherwise inka synthesizes a generic `bundle-esm` conversion.
- **The `inka-patcher` binary** — discovered at `$INKA_PATCHER` → `<dir of inka binary>/inka-patcher`. Build it with `cargo build --release` inside `crates/inka-patcher` (heavy rolldown stack; use the big-disk cargo home/target) and keep it next to the inka binary.

Colocate both for a prefix in one step:

```sh
scripts/install-patches.sh <prefix-dir> [<path-to-built-inka-patcher>]
```

If a conversion is needed and either piece is missing, `inka add` fails cleanly and names the affected package(s) plus the install command above (and points at `INKA_PATCHES` when no spec directory is discoverable). Converted packages that can't be handled automatically fail with a scaffold error telling you the exact `patches/<name>/<version>/patch.json` to author — or you can keep the package in the default store instead.

## Building the real runtime

`crates/inka-runtime` requires the heavy Deno dependency tree (V8, wgpu, …) and a one-time ~10–15 min build plus a snapshot-generation step. Point it at a roomy disk if your system drive is full:

```sh
CARGO_TARGET_DIR=/big/disk/inka-target cargo build --release -p inka-runtime
```

Install the result as a tuple:

```sh
cp target/release/libinka_runtime.so ~/.inka-runtime/libinka_runtime-0.266.0.so
```

## Packaging a `.deb` (toolchain)

`scripts/build-deb.sh` builds a standalone, upgrade-aware `.deb` that installs
the toolchain (`inka`, `inka-launcher`, `inka-patcher`, curated `patches/`) into
`/usr/lib/inka` with a `/usr/bin/inka` symlink — deliberately **without** a
runtime or store, which stay per-user under `~/.inka-runtime` via
`inka install <ver> --from <base>`. See `docs/deployment.md` for the layout,
upgrade/versioning rules, and app/container deployment guidance.

## Measured on Linux x86_64 (deno_runtime 0.266.0)

`console.log("hello world")`:

| | Artifact | Cold start |
|---|---|---|
| `deno compile` | 99.5 MB | ~0.02 s |
| `bun build --compile` | 90 MB | ~0.03 s |
| **inka artifact** | **0.35 MB** | **~0.04 s** (with snapshot) |
| Shared `libinka_runtime-0.266.0.so` | 96 MB, once per machine | — |

Without the embedded snapshot, inka cold-starts at ~0.6 s; the snapshot brings it to ~0.04 s. The 96 MB runtime is the price of *not* baking an engine into every file, and it is shared by every inka executable on the machine.

## Why not just bundle / why not a system-wide stable engine?

- Bundling the runtime into each artifact (like `deno compile`) gives ~100 MB per file and ties every artifact to one engine.
- A distro-wide "system JS engine" with a frozen ABI freezes the language; V8/JIT engines have no stable ABI and snapshots are build-locked.
- inka keeps artifacts source-portable: your code is interpreted/JIT-ed at run time against whatever tuple you point at, so runtime upgrades never require rebuilding apps.

## Current limits / roadmap

- `node:` built-ins resolve with or without the prefix (`vm` ≡ `node:vm`); bare `npm:`/`jsr:` package names resolve **only** against the local package store (`inka pkg seed`), and `http(s):` module imports are rejected — the engine is offline by construction.
- `.tsx`/`.jsx` are not supported yet; `--transpile` works for single- and multi-file `.ts/.mts/.cts` (JSX entries error).
- Store *resolution* is fully general, but *execution* still depends on the engine's Node compatibility: the engine is ESM-only and cannot run CommonJS (no `require`/`createRequire` support). The shipped store therefore **patches CJS leaves at snapshot time** — `crates/inka-patcher` (an embedded rolldown, run by `pkg snapshot`) bundles CJS packages (currently `ws`, `undici`, `mime`) to pure engine-viable ESM and neuters env reads at import (`msgpackr`) so no `allow-env` is needed. Any CommonJS that still reaches the loader is rejected cleanly. Pure-ESM packages like `effect` itself need nothing. Verified offline through `@effect/platform-node` (`NodeRuntime.runMain`): an HTTP self-fetch round-trip (`NodeHttpServer` + `undici`) and a WebSocket echo over the platform upgrade adapter.
- Native `.node` addons (e.g. the optional `@effect/platform-node/NodeFileSystem/ParcelWatcher` watch path, which uses `@parcel/watcher`) are **not supported** — the engine cannot load N-API binaries. The default `NodeFileSystem` uses `node:fs` and is unaffected; opting into a native-watcher module fails cleanly (CommonJS). Real native-addon support would be Option-B-class (deno node-services/FFI) engine work.
- Successful runs are silent; set `INKA_DEBUG=1` to see launcher diagnostics (`resolved …`, `runtime … reports: …`) on stderr. Genuine errors always print with a `[inka]` prefix.
- `inka install` verifies SHA-256 integrity but not authenticity — production distribution should sign checksums (e.g. minisign) and pin a trust anchor.
- HTTP fetch of runtimes shells out to `curl` (TLS handled by curl); a native TLS client would remove that dependency.

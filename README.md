# dex

Tiny single-file executables for JavaScript/TypeScript that run on a **shared, tuple-versioned Deno runtime** instead of bundling one into every binary.

## How it works: the launcher, the manifest, and the embed model

Normal "compiled" JS binaries (`deno compile`, `bun build --compile`) work by **fusing the JS engine into the file**. Result: a ~100 MB "Hello World". The engine (V8 + snapshot + ICU) is huge, and every artifact carries its own copy.

The dex idea: **don't put the engine in the file.** Put a *tiny host* in the file, and have the engine be one shared library installed once per machine that any dex file can load.

So there are exactly three things in play:

| Piece | What it is | Where it lives | Size |
|---|---|---|---|
| **Launcher** | A small native program compiled from Rust (`crates/launcher`) | **copied inside every artifact** | ~355 KB |
| **Manifest** | A few lines of text describing the artifact | appended inside the artifact | ~50 B |
| **Runtime** | `libdeno_runtime-<version>.so` — the real Deno engine (from `crates/runtime-deno`) | installed *once* per machine | ~96 MB |

### The artifact file is just a concatenation

`dex build app.js` glues bytes together and writes one file. Inside the artifact the bytes are, in order:

```
[ launcher executable bytes ]   ← the compiled Rust launcher
[ your source (app.js bytes) ]  ← the program
[ your manifest bytes        ]  ← what it needs and may do
[ footer: "DEXFOOT2" + length of source + length of manifest ]
```

The footer is the last 24 bytes: an 8-byte magic string (`DEXFOOT2`) plus two 8-byte lengths. That is the only "clever" part — it is how the launcher finds its data when it runs.

Why does this work? Because an executable doesn't have to be *only* machine code. The OS runs the code at the start of the file and ignores trailing garbage. Appending text to a compiled binary is safe. That is the entire "embed" model: **the source and manifest are just extra bytes stuck onto the end of the launcher.**

### What the launcher does when you run `./app`

It has no idea what your program does — it is a generic host. At startup it:

1. **Finds itself** via `/proc/self/exe` (the path of the running file) and reads its own bytes.
2. **Locates the trailer** — reads the last 24 bytes, checks the magic, gets the two lengths, and slices out the source and the manifest from the middle.
3. **Reads the manifest** to learn what runtime version it needs (`runtime=…`), what to call the entry file (`module=…`), whether to pin roll-forward (`tested-against=…`), and what permissions to grant (`permissions=…`, `allow-*`, `deny-*`).
4. **Searches for a runtime** on the machine (`$DENO_RUNTIME_HOME` → `~/.deno-runtime` → `/usr/local/lib/deno-runtime`), scans for `libdeno_runtime-*.so` files, parses versions out of the filenames, and picks the newest one that satisfies the manifest (≥ floor, ≤ `tested-against`).
5. **`dlopen`s that `.so`** (like loading a plugin) and calls a fixed C function, handing it the embedded source, the command-line args, and the permissions string.
6. **Stays alive as the host process.** The runtime starts V8 (fast — it has a pre-baked snapshot), creates a Deno worker, stages the source, transpiles it if it is `.ts`, executes it, and runs the event loop until the program finishes.
7. **Returns the exit code.** stdin/stdout/stderr are inherited, so `console.log` and friends just work.

The launcher never executes your code. It is a courier: it carries the source to the engine and reports back the exit code. Your code is interpreted/JIT-compiled **at run time by the runtime**, not at build time (unless you pass `--transpile`).

### Why the manifest exists

The runtime is versioned and shared, but different artifacts may need different things from it. The manifest is the artifact's **declaration of intent**, read by the launcher at run time:

```
runtime=deno_runtime>=0.266.0     # I need at least this engine
tested-against=0.266.0            # optional: never auto-run on something newer
module=app.js                     # display name for my entry source
allow-read=/etc,./data            # grant these file reads (deny everything else)
```

Without a manifest the launcher wouldn't know which of the possibly-several installed `.so` files to load, or under what permissions.

### What this buys

- **Small files.** The artifact is ~355 KB because it only carries *transport* (launcher) + *cargo* (source + manifest). The ~96 MB engine is a per-machine shared library, loaded by whatever file you run.
- **Runtime upgrades don't touch your files.** Install `libdeno_runtime-0.267.0.so` on the machine and every artifact whose manifest says `>= 0.266.0` automatically starts using it — same source, same launcher, new engine underneath. Your code was never compiled against an engine, so nothing needs rebuilding.
- **Policy travels with the file** (permissions are embedded at build time) while **the engine stays shared**.

**The core guarantee:** your app artifact is *never rebuilt or re-shipped* when the runtime updates. You install a newer runtime once per machine; every dex executable on that machine keeps running against it.

The mental flip versus what you're used to: *the file you distribute is not the program.* The program is your source text; the file is a self-describing envelope that picks an engine off the shelf at run time and hands it your code.

```
┌────────────── your artifact (single file, ~0.35 MB) ──────────────┐
│  Rust launcher: trailer parse → manifest → tuple resolve → dlopen  │
│  [app source + manifest ← appended, magic+length footer at EOF]   │
└──────────────────────────────────┬─────────────────────────────────┘
                        dlopen (libloading)
┌──────────────────────────────────▼─────────────────────────────────┐
│  libdeno_runtime-0.266.0.so  (Rust cdylib: deno_runtime 0.266.0)  │
│  [V8 engine + embedded startup snapshot + ICU — build-locked]      │
└────────────────────────────────────────────────────────────────────┘
```

## Workspace layout

| Crate | Role |
|---|---|
| `crates/launcher` | Thin native host: parses the appended trailer, resolves a tuple, `dlopen`s it, runs your module |
| `crates/runtime-stub` | Tiny fake `.so` exporting the same C ABI — used to develop/test the launcher cheaply |
| `crates/runtime-deno` | Real runtime: `deno_runtime` behind the frozen C ABI, with a V8 startup snapshot embedded at build time |
| `crates/dex` | Companion CLI: `dex build` (pack launcher + source + manifest into an artifact), `dex install <version>` (checksum-gated runtime distribution), `dex list` |

## The frozen C ABI (identical in stub and real runtime)

```c
const char* dex_runtime_version(void);
void*       dex_runtime_create(void);
int  dex_runtime_run_module(void*, const char* specifier,
                            const char* source, size_t len,
                            int argc, char** argv,
                            int* exit_code, char** err_msg);
int  dex_runtime_run_module_perm(void*, const char* specifier,   /* additive */
                            const char* source, size_t len,
                            int argc, char** argv,
                            int* exit_code, char** err_msg,
                            const char* perms);
void dex_runtime_destroy(void*);
```

`dex_runtime_run_module_perm` is the additive, permission-aware entry point; `perms` is a newline-joined string of permission lines (null/empty = deny-by-default). The legacy `dex_runtime_run_module` is kept for older runtimes and behaves the same as `_perm` with empty permissions (deny-by-default) on current runtime builds. If an artifact declares permissions but the installed runtime lacks the `_perm` symbol, the launcher **fails closed** (exit 4) instead of silently running allow-all.

Everything else (Deno.\*, Web APIs, the event loop) lives inside the `.so` and is invisible to the ABI.

## Runtime versioning ("tuples")

One installed runtime file = one version: `libdeno_runtime-<version>.so`. A `deno_runtime` crate version pins a specific V8 + snapshot + ICU, so the *tuple* collapses to a single version number.

Search order: `$DENO_RUNTIME_HOME` → `~/.deno-runtime` → `/usr/local/lib/deno-runtime`. `DEX_RUNTIME=/path/to/lib.so` forces one specific file.

Manifest policy:

```
# key=value
runtime=deno_runtime>=0.266.0     # minimum floor
tested-against=0.266.0            # optional cap: never auto-use newer than tested
module=main.js                    # display specifier
```

- No cap → newest installed tuple ≥ floor wins (roll-forward).
- With cap → newest installed tuple ≤ cap that is ≥ floor.
- No match → clear error naming the requirement and search paths (exit 3).

## Permissions

Permissions are baked into the artifact at `dex build` via the manifest — the executable is always launched plain (`./app ...`), never with `--allow-*` flags, and prompting is disabled. Categories: `read`, `write`, `net`, `env`, `run`, `sys`, `ffi`.

**Deny by default:** an artifact grants nothing unless its manifest says so.

```
runtime=deno_runtime>=0.266.0
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
- **Relative paths resolve at run time** against the process cwd (Deno semantics). Resolving them against the artifact's build location is a noted future option.
- **Policy is a property of the runtime build:** runtimes built before the deny-by-default change ran permission-less artifacts allow-all; current builds deny. Rebuild old artifacts against the current runtime to inherit the new default (or declare `permissions=all`).

## Quickstart

```sh
cargo build --release -p dex -p launcher

# 1. write your program
cat > app.js <<'EOF'
console.log(`hello ${Deno.args[0] ?? "world"} from deno ${Deno.version.deno}`);
EOF

# 2. manifest (same directory as the source)
printf 'runtime=deno_runtime>=0.266.0\nmodule=app.js\n' > app.manifest

# 3. pack  (manifest auto-found as app.manifest; output auto-derived as "app")
./target/release/dex build app.js
# ...or be explicit:
./target/release/dex build app.js --manifest app.manifest -o myapp

# 4. run (needs a compatible runtime installed)
./myapp kook
```

`dex build` finds the launcher automatically: `$DEX_LAUNCHER`, else `dex-launcher` next to the `dex` binary (so build `-p launcher` too and keep them together). Defaults: source = positional arg (or `-s/--source`), output = source name without its extension, manifest = `<source-stem>.manifest` then `dex.manifest` in the current directory.

### TypeScript

Single-file TypeScript entries are supported as-is — no extra steps:

```sh
# app.ts (types, interfaces, generics, top-level await all fine)
./target/release/dex build app.ts        # -> ./app  (module=app.ts is set for you)
./app kook
```

Two ways to compile TS → JS:

- **Runtime transpile (default):** the artifact keeps your `.ts` source; the shared runtime `.so` transpiles it at load using the tuple's own TS compiler (so TS semantics track the runtime, not your machine).
- **Build-time transpile:** `dex build app.ts --transpile` compiles to JS while packing, so the artifact ships pure JavaScript.

`dex build` normalizes the manifest for you: if it has no `module=` line, one matching the source is appended (`.ts` preserved for runtime transpile, `.js` after `--transpile`); an explicit `module=` that contradicts the packed code's language prints a warning.

Install a runtime on a machine (requires a `<file>.sha256` sidecar or `--sha256 <hex>`; add `--insecure` to skip):

```sh
dex install 0.266.0 --from https://your-registry.example/runtimes
dex list
```

## Building the real runtime

`crates/runtime-deno` requires the heavy Deno dependency tree (V8, wgpu, …) and a one-time ~10–15 min build plus a snapshot-generation step. Point it at a roomy disk if your system drive is full:

```sh
CARGO_TARGET_DIR=/big/disk/dex-target cargo build --release -p runtime-deno
```

Install the result as a tuple:

```sh
cp target/release/libdex_runtime_deno.so ~/.deno-runtime/libdeno_runtime-0.266.0.so
```

## Measured on Linux x86_64 (deno_runtime 0.266.0)

`console.log("hello world")`:

| | Artifact | Cold start |
|---|---|---|
| `deno compile` | 99.5 MB | ~0.02 s |
| `bun build --compile` | 90 MB | ~0.03 s |
| **dex artifact** | **0.35 MB** | **~0.04 s** (with snapshot) |
| Shared `libdeno_runtime-0.266.0.so` | 96 MB, once per machine | — |

Without the embedded snapshot, dex cold-starts at ~0.6 s; the snapshot brings it to ~0.04 s. The 96 MB runtime is the price of *not* baking an engine into every file, and it is shared by every dex executable on the machine.

## Why not just bundle / why not a system-wide stable engine?

- Bundling the runtime into each artifact (like `deno compile`) gives ~100 MB per file and ties every artifact to one engine.
- A distro-wide "system JS engine" with a frozen ABI freezes the language; V8/JIT engines have no stable ABI and snapshots are build-locked.
- dex keeps artifacts source-portable: your code is interpreted/JIT-ed at run time against whatever tuple you point at, so runtime upgrades never require rebuilding apps.

## Current limits / roadmap

- No `node:`/`npm:` module resolution (the npm trait slots are inert; non-npm code is unaffected).
- TypeScript support is single-file: the artifact embeds one entry module, so relative imports of sibling files are not yet supported (multi-file embedding + a runtime module loader is on the roadmap). `.tsx`/`.jsx` are not supported yet.
- Successful runs are silent; set `DEX_DEBUG=1` to see launcher diagnostics (`resolved …`, `runtime … reports: …`) on stderr. Genuine errors always print with a `[dex]` prefix.
- `dex install` verifies SHA-256 integrity but not authenticity — production distribution should sign checksums (e.g. minisign) and pin a trust anchor.
- HTTP fetch of runtimes shells out to `curl` (TLS handled by curl); a native TLS client would remove that dependency.

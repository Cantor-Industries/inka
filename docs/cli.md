# CLI reference

```
inka build   [source] [-s <file>] [-o <file>] [--runtime <spec>] [--tested-against <ver>] [-P <name>] [--transpile] [--embed-dir] [--vendor-closure|--no-vendor] [--no-node-modules]
inka run     [-A] [-P[=name]] [--allow-<cat>[=list]|--deny-<cat>[=list]]... <file> [args...]
inka install [pkg[@ver]...] [--force] [--prod]
inka update  [<version>] [--from <dir-or-url>] [--sha256 <hex>] [--insecure] [--home <dir>]
             [--no-toolchain|--toolchain-only|--store-only] [--no-runtime] [--no-store]
inka add     <pkg[@ver]> [--force]
inka remove  <pkg>
inka vendor  list|status|release|ignore
inka list    [--home <dir>]
inka doctor
```

## `build`

Pack the launcher + your source (and its import closure) + a manifest into one
executable. Permissions bake only from explicit build-intent sources; otherwise
the artifact is deny-by-default. See [Build](build.md).

## `run`

Execute a `.ts`/`.js` file through the installed runtime without building.
Permissions are deny-by-default; `-A` allows all, `-P[=name]` applies a named
config set, `--allow-<cat>[=list]` / `--deny-<cat>[=list]` are granular
(`cat`: `read|write|net|env|run|sys|ffi`). `--runtime <ver>` picks a specific
tuple; `--` ends options. See [Run](run.md).

## `install`

Vendor dependencies into the project's `vendored/`:

- no arguments: every root declared in `package.json` `dependencies` and
  `deno.json` `imports`;
- with arguments: those packages (same as `inka add`).

Already-vendored or store-provided packages are skipped (idempotent). Ranges are
resolved and pinned exactly. See [Packages & the store](packages.md).

## `update`

Reconcile the toolchain and shared engine with the release channel:

- no version: read `<base>/versions.json`; self-update the toolchain when an
  installer-managed install is present (`VERSION` marker), install only the
  runtime when it is missing or behind, and sync the store snapshot
  (`sha256`-gated). Never downgrades; older runtime tuples are kept.
- `<version>`: install that exact runtime tuple (offline/pinned).

`--no-toolchain`/`--toolchain-only` control the toolchain; `--store-only`
provisions just the package store; `--no-runtime`, `--no-store` skip individual
components.

Base resolution: `--from` → `$INKA_RELEASE_BASE` → `$INKA_RT_SOURCE` → the
built-in GitHub latest-release URL. `--sha256` pins a checksum; `--insecure`
skips verification; `--home <dir>` sets the runtime install dir. See
[Install & upgrade](install-and-upgrade.md).

## `add` / `remove` / `vendor`

Per-project vendoring into a real npm tree (`vendored/node_modules/…`). `add`
re-resolves the whole root set and vendors one package; `remove` drops a root and
re-resolves; `vendor list|status` inspect roots and store coverage; `vendor
release|ignore` set the git posture of `vendored/`.

## `list` / `doctor`

`list` prints installed runtime tuples (across all search dirs; `--home` narrows
to one). `doctor` prints a full diagnostic report.

## Environment

| Variable | Effect |
|---|---|
| `XDG_DATA_HOME` | base for `inka/runtime` and `inka/store` (default `~/.local/share`) |
| `INKA_RUNTIME_HOME` | override the per-user runtime dir |
| `INKA_STORE` | override the default store dir |
| `INKA_VENDOR` | override the vendored root |
| `INKA_RELEASE_BASE` / `INKA_RT_SOURCE` | override the update channel base |
| `INKA_LAUNCHER` | path to `inka-launcher` for `build` |
| `INKA_DEBUG` | verbose runtime/resolution logging |

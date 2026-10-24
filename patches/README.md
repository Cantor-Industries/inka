# Vendored code patches

This directory tracks local changes to code copied or adapted from upstream
projects. The local copy stays committed (so builds are offline and
deterministic); the patch series is the reviewable source of truth for the
difference from upstream.

## Layout

Each unit is a directory directly under `patches/`:

```text
patches/<unit>/
  metadata.env         # sourceable shell metadata
  series               # ordered patch paths, one per line
  <version>/           # optional version/tag subdirectory
    0001-*.patch       # unified diff, applied with `patch -p1`
```

`metadata.env` fields:

| Field | Meaning |
|---|---|
| `UNIT` | Stable name; also the `--only` selector |
| `KIND` | `file` or `crate` |
| `UPSTREAM_REPO` | e.g. `denoland/deno` |
| `UPSTREAM_REF` | tag/version the local copy is based on |
| `UPSTREAM_PATH` | path inside the upstream repo (file units) |
| `UPSTREAM_URL` | exact source URL or crates.io download URL |
| `BASE_SHA256` | sha256 of the upstream file, or of the `.crate` tarball |
| `CRATE` / `VERSION` | crate name/version (crate units only) |
| `LOCAL_PATH` | path in this repo |
| `LOCAL_SHA256` | sha256 of the current local file (optional) |
| `EXCLUDE` | comma-separated paths to ignore when diffing a crate |

Patch paths are relative to the patch root: `crate` units use crate-root paths
(for example `ops/fs.rs`), while `file` units use repo-relative paths (for
example `crates/inka-runtime/src/desktop_js.rs`). `series` entries may include a
version subdirectory.

## Verify

```sh
scripts/ci/vendor-patches.sh --list
scripts/ci/vendor-patches.sh --only deno_node
scripts/ci/vendor-patches.sh                 # all units
```

The script fetches the exact upstream source by default, verifies the base
sha256, applies the series, and diffs against the local copy. For offline use:

```sh
scripts/ci/vendor-patches.sh --only deno_node --upstream /path/to/deno_node-0.198.0
```

`--upstream` for a `file` unit is the upstream file itself; for a `crate` unit it
is the extracted crate root.

## Deferred units

Files whose upstream base is unclear or whose local divergence is too large for a
maintainable patch are listed in `DEFERRED.md` and tracked in the corresponding
GitHub issue. They are not part of the current patch series.

## Updating a unit

1. Fetch the new upstream base for the target tag/version.
2. Re-apply the existing series with `patch -p1`; resolve rejected hunks.
3. Update the local file/tree.
4. Regenerate the series from the upstream base.
5. Update `BASE_SHA256`, `UPSTREAM_REF`, `LOCAL_SHA256`, and any notes.
6. Run `scripts/ci/vendor-patches.sh --only <unit>`.
7. Commit the local change, metadata, and patch together.

When upstream contains the change, remove the unit directory, the local vendor
copy if applicable, and the associated `[patch.crates-io]`/pin references.

# Local `deno_node` patch

Base version: **0.198.0** (the latest published crate, shipped with Deno
v2.9.8 / `deno_runtime 0.268.0`).

## Patch

`ops/fs.rs` adds a narrow fallback for **`UvOwned`** file descriptors: fds
created for `child_process` stdio pipes and owned by libuv, for which
`deno_io::FdTable::get()` intentionally returns `None`.

`op_node_fs_read_sync` / `op_node_fs_write_sync` now:

- detect `FdTable::contains(fd) && get(fd).is_none()`;
- read/write the raw fd directly with `libc`, without `dup`, registration, or
  ownership transfer;
- retry `EINTR`, poll for readiness on `EAGAIN`, and loop on partial writes;
- keep the existing behavior for every other fd and for `position >= 0`.

This makes TypeScript 7's synchronous `tsc` RPC transport work. It does **not**
add general raw-fd access, a permission flag, or an `FdTable` registration path.

## Upstream

Deno PR #33055 is a broader, still-unmerged effort for native-addon/inherited
raw fds. As written it does not handle `UvOwned` (the review flags this), and it
adds a permission model that we do not need for this case.

Remove this vendor directory once a released `deno_runtime` contains an
equivalent fix; the Cargo patch and this note should go together.

The containing runtime tuple must advance before this ships (for example
`0.268.1` → `0.268.2-beta.1`); `crates/inka-runtime/runtime-version` is
intentionally unchanged on the feature branch until the release build is cut.

> Formal patch tracking (base hashes, patch series, upstream links) is a
> follow-up after this runtime change lands.

# deno_node 0.198.0 — UvOwned sync fd I/O

Base: `deno_node 0.198.0`, the latest published crate (Deno v2.9.8,
`deno_runtime 0.268.0`). The crates.io tarball checksum is recorded in
`metadata.env`.

Patches:

- `0001-uv-owned-sync-io.patch`: `op_node_fs_read_sync` / `op_node_fs_write_sync`
  fall back to raw `libc::read`/`write` for fds that `deno_io::FdTable` tracks as
  `UvOwned` (child-process stdio pipes). It retries `EINTR`, polls on `EAGAIN`,
  loops on partial writes, and only applies when `position < 0`. It does not
  dup, register, cache, close, or take ownership of the fd; Windows keeps the
  named-pipe path.
- `0002-msvc-llhttp-c99.patch`: skip `.std("c99")` for MSVC, whose compiler
  warns (D9002) about the GCC-style flag.
- `0003-silence-fetch-update-deprecation.patch`: allow the deprecated
  `Atomic::fetch_update` on newer Rust while keeping compatibility with older
  toolchains.

## Upstream

- Deno PR #33055 is the broader raw-fd effort. It is unmerged, adds a new
  permission model, and as written does not handle `UvOwned`.
- A focused upstream fix should add a spec test for synchronous
  `child_process` stdio fd I/O.

Current release tuple: `0.268.2-beta.1`.

Remove this unit (and `[patch.crates-io] deno_node` in the workspace
`Cargo.toml`) once a released `deno_runtime` contains an equivalent fix.

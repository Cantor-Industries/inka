# Windows platform helpers (Deno 2.9.8)

Base: Deno v2.9.8 `cli/util/windows.rs`.

Only `process_image_path` (stale-temp-tree liveness) and
`ensure_stdio_open` (GUI-subsystem stdio repair) are kept. The patch removes the
Restart Manager/cache-locking helpers and rewrites the retained functions in
inka's style.

Update procedure: fetch the new `cli/util/windows.rs` at the target Deno tag,
re-apply the series, port changes to the two retained helpers, and regenerate
the patch.

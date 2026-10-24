# Desktop JS surface (Deno 2.9.8)

Base: Deno v2.9.8 `cli/rt/desktop.rs`.

Local file `crates/inka-runtime/src/desktop_js.rs` keeps the desktop JS surface
and the auto-update/error-reporting builders needed by inka, drops the Rust
op-state wiring that inka implements in `desktop_api.rs`, and formats the Rust
helpers in inka style. The embedded JS string is otherwise sourced from Deno.

Update procedure: fetch the new `cli/rt/desktop.rs` at the target Deno tag,
re-apply the series, port any new JS/helper changes, and regenerate the patch.
`scripts/ci/desktop-js-pins.sh` still checks the Deno/laufey pins and the
required op symbols.

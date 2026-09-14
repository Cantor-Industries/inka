// The stub embeds `INKA_STUB_VERSION`. Re-run when it changes and pass it to the
// crate via `rustc-env`, so changing the version actually recompiles the stub
// (a bare `rerun-if-env-changed` is not enough: the build script output must
// change too).
fn main() {
    println!("cargo:rerun-if-env-changed=INKA_STUB_VERSION");
    let v = std::env::var("INKA_STUB_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    println!("cargo:rustc-env=INKA_STUB_VERSION={v}");
}

// Bake the release identity into the launcher binary. Mirrors
// `crates/inka/build.rs`; the launcher only needs `INKA_BUILD_VERSION` for its
// `--version` output.
fn main() {
    for key in ["INKA_BUILD_VERSION", "INKA_BUILD_COMMIT"] {
        println!("cargo:rerun-if-env-changed={key}");
        if let Ok(val) = std::env::var(key) {
            let val = val.trim();
            if !val.is_empty() {
                println!("cargo:rustc-env={key}={val}");
            }
        }
    }
}

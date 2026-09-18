// Bake the release identity into the binary. The release pipeline sets
// `INKA_BUILD_VERSION` (the exact release string, e.g. `0.8.1-beta.2`) and
// `INKA_BUILD_COMMIT` (short hash). A plain `cargo build` leaves both unset,
// which the CLI treats as a dev build (it falls back to `CARGO_PKG_VERSION`).
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

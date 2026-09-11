// inka: companion tooling for inka artifacts.
//
//   inka build [source] [-s|--source <file>] [-o|--output <file>]
//               [--runtime <spec>] [--tested-against <ver>] [-P <name>]
//               [--transpile] [--embed-dir] [--vendor-closure] [--no-vendor]
//   inka update [<version>] [--from <dir-or-url>] [--sha256 <hex>]
//                           [--insecure] [--home <dir>]
//   inka install [pkg[@ver]...]   vendor this project's dependencies
//   inka list [--home <dir>]

mod build;
mod config;
mod embed;
mod pkg;
mod run;
mod transpile;
mod update;
mod vendor;

use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) const FILENAME_PREFIX: &str = "libinka_runtime-";
pub(crate) const FILENAME_SUFFIX: &str = ".so";
pub(crate) const RESOLVER_PREFIX: &str = "libinka_resolver-";
pub(crate) const RESOLVER_SUFFIX: &str = ".so";
/// Resolver version used when fetching from a URL base that has no directory
/// listing (and $INKA_RESOLVER_VERSION is unset).
pub(crate) const DEFAULT_RESOLVER_VERSION: &str = "1.0.0";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Version(pub(crate) u64, pub(crate) u64, pub(crate) u64);

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

pub(crate) fn parse_version(s: &str) -> Option<Version> {
    let s = s.trim();
    let mut parts = s.split('.');
    let a = parts.next()?.parse().ok()?;
    let b = parts.next().unwrap_or("0").parse().ok()?;
    let c = parts.next().unwrap_or("0").parse().ok()?;
    // reject trailing garbage like "0.0.0-stub" for install targets
    if parts.next().is_some() {
        return None;
    }
    Some(Version(a, b, c))
}

fn usage() -> ! {
    eprintln!(
        "usage:\n  inka build [source] [-s|--source <file>] [-o|--output <file>] [--runtime <spec>] [--tested-against <ver>] [-P <name>] [--transpile] [--embed-dir] [--vendor-closure] [--no-vendor]\n  inka update [<version>] [--from <dir-or-url>] [--sha256 <hex>] [--insecure] [--home <dir>]\n  inka install [pkg[@ver]...]  vendor this project's dependencies (or `inka add`)\n  inka list [--home <dir>]\n  inka add <pkg[@ver]>        vendor a package not in the default store\n  inka remove <pkg>           un-vendor a package (+ prune orphaned vendored deps)\n  inka vendor list|status|release|ignore\n  inka doctor                 print a diagnostic report (runtimes, resolver, store, vendored)\n  inka run [-A] [-P[=name]] [--allow-<cat>[=list]|--deny-<cat>[=list]]... <file> [args...]\n                             execute a ts/js file via the installed runtime\n  inka --version, -V          print the inka toolchain version"
    );
    std::process::exit(2);
}

/// `$XDG_DATA_HOME` when set (non-empty, absolute), else `$HOME/.local/share`,
/// else the current directory.
fn data_root(home: Option<&std::ffi::OsStr>, xdg: Option<&std::ffi::OsStr>) -> PathBuf {
    if let Some(x) = xdg {
        if !x.is_empty() {
            let p = PathBuf::from(x);
            if p.is_absolute() {
                return p;
            }
        }
    }
    if let Some(h) = home {
        if !h.is_empty() {
            return PathBuf::from(h).join(".local/share");
        }
    }
    PathBuf::from(".")
}

fn data_root_now() -> PathBuf {
    data_root(env::var_os("HOME").as_deref(), env::var_os("XDG_DATA_HOME").as_deref())
}

/// Per-user inka data dir: `$XDG_DATA_HOME/inka` (`~/.local/share/inka`).
pub(crate) fn inka_data_dir() -> PathBuf {
    data_root_now().join("inka")
}

/// Per-user runtime dir: `<data>/inka/runtime`.
pub(crate) fn user_runtime_dir() -> PathBuf {
    inka_data_dir().join("runtime")
}

/// Per-user default package store: `<data>/inka/store`.
pub(crate) fn default_store_dir() -> PathBuf {
    inka_data_dir().join("store")
}

/// Where a runtime install/update writes when `--home` is not given:
/// `INKA_RUNTIME_HOME` else the per-user XDG runtime dir.
pub(crate) fn default_install_dir() -> PathBuf {
    if let Ok(h) = env::var("INKA_RUNTIME_HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    user_runtime_dir()
}

/// Directories searched for installed runtime/resolver `.so` files, in order:
/// `INKA_RUNTIME_HOME`, then the per-user XDG runtime dir.
pub(crate) fn runtime_search_dirs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(h) = env::var("INKA_RUNTIME_HOME") {
        if !h.is_empty() {
            out.push(PathBuf::from(h));
        }
    }
    let user = user_runtime_dir();
    if !out.contains(&user) {
        out.push(user);
    }
    out
}

fn main() {
    // Restore the default SIGPIPE disposition: piping output into `head`/`grep -q`
    // closes the pipe, and the default action (terminate quietly) is preferable to
    // Rust's panic-on-EPIPE, which aborts with `panic = "abort"`.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }
    match args[0].as_str() {
        "--version" | "-V" => println!("inka {}", env!("CARGO_PKG_VERSION")),
        "build" => build::cmd_build(&args[1..]),
        "update" => update::cmd_update(&args[1..]),
        "list" => cmd_list(&args[1..]),
        "install" => vendor::cmd_install(&args[1..]),
        "add" => vendor::cmd_add(&args[1..]),
        "remove" => vendor::cmd_remove(&args[1..]),
        "vendor" => vendor::cmd_vendor(&args[1..]),
        "doctor" => cmd_doctor(&args[1..]),
        "run" => run::cmd_run(&args[1..]),
        "internal" => cmd_internal(&args[1..]),
        _ => usage(),
    }
}

/// Hidden release-time tooling (not advertised in `usage`).
fn cmd_internal(args: &[String]) {
    match args.first().map(String::as_str) {
        Some("snapshot-store") => pkg::cmd_snapshot_store(&args[1..]),
        _ => {
            eprintln!("error: unknown internal command");
            std::process::exit(2);
        }
    }
}

// ---- fetch helpers ---------------------------------------------------------

/// Fetch `<base>/<file>` plus `<base>/<file>.sha256` when available.
/// `base` may be a local directory path or an http(s) URL.
pub(crate) fn fetch_with_sidecar(base: &str, file: &str) -> Result<(Vec<u8>, Option<String>), String> {
    let is_url = base.starts_with("http://") || base.starts_with("https://");
    let main = fetch_one(base, file, is_url)?;
    let sidecar = fetch_optional(base, &format!("{file}.sha256"), is_url)?;
    Ok((main, sidecar))
}

/// Fetch `<base>/<file>` as UTF-8 text (for release metadata like versions.json).
pub(crate) fn fetch_text(base: &str, file: &str) -> Result<String, String> {
    let is_url = base.starts_with("http://") || base.starts_with("https://");
    let bytes = fetch_one(base, file, is_url)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn fetch_one(base: &str, file: &str, is_url: bool) -> Result<Vec<u8>, String> {
    if is_url {
        fetch_http(&format!("{}/{}", base.trim_end_matches('/'), file))
    } else {
        let p = PathBuf::from(base).join(file);
        fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))
    }
}

/// Fetch an optional sidecar: `Ok(None)` means the file is genuinely absent
/// (local ENOENT, or an HTTP error response such as 404); connection/DNS/TLS/
/// timeout failures are surfaced as errors rather than hidden as "no checksum".
fn fetch_optional(base: &str, file: &str, is_url: bool) -> Result<Option<String>, String> {
    if is_url {
        let url = format!("{}/{}", base.trim_end_matches('/'), file);
        Ok(http_get_optional(&url)?.map(|b| String::from_utf8_lossy(&b).into_owned()))
    } else {
        let p = PathBuf::from(base).join(file);
        match fs::read(&p) {
            Ok(b) => Ok(Some(String::from_utf8_lossy(&b).into_owned())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", p.display())),
        }
    }
}

/// Like `fetch_http`, but `Ok(None)` when the server answers with an HTTP error
/// (curl exit 22 / wget exit 8), while connection-level failures are errors.
fn http_get_optional(url: &str) -> Result<Option<Vec<u8>>, String> {
    let mut cmd = Command::new("curl");
    if url.starts_with("https://") {
        cmd.args(["--proto", "=https", "--tlsv1.2"]);
    }
    match cmd
        .args(["-fsSL", "--connect-timeout", "30", "--max-time", "900", url])
        .output()
    {
        Ok(out) if out.status.success() => return Ok(Some(out.stdout)),
        Ok(out) if out.status.code() == Some(22) => return Ok(None), // HTTP error
        Ok(_) => {}                                                 // try wget
        Err(_) => {}                                                // curl missing
    }
    match Command::new("wget").args(["-qO-", "--timeout=30", url]).output() {
        Ok(out) if out.status.success() => Ok(Some(out.stdout)),
        Ok(out) if out.status.code() == Some(8) => Ok(None), // server error
        Ok(out) => Err(format!(
            "failed to download {url} (wget exit {:?})",
            out.status.code()
        )),
        Err(e) => Err(format!("failed to download {url}: {e}")),
    }
}

/// Download over HTTP(S), preferring `curl` and falling back to `wget`.
fn fetch_http(url: &str) -> Result<Vec<u8>, String> {
    match curl_get(url) {
        Ok(bytes) => Ok(bytes),
        Err(curl_err) => match wget_get(url) {
            Ok(bytes) => Ok(bytes),
            Err(wget_err) => Err(format!(
                "failed to download {url}\n  curl: {curl_err}\n  wget: {wget_err}"
            )),
        },
    }
}

fn curl_get(url: &str) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new("curl");
    // Pin TLS for https (avoid downgrade); allow plain http for local mirrors.
    if url.starts_with("https://") {
        cmd.args(["--proto", "=https", "--tlsv1.2"]);
    }
    let out = cmd
        .args(["-fsSL", "--connect-timeout", "30", "--max-time", "900", url])
        .output()
        .map_err(|e| format!("curl not available ({e})"))?;
    if !out.status.success() {
        return Err(format!("curl exited with {}", out.status));
    }
    Ok(out.stdout)
}

fn wget_get(url: &str) -> Result<Vec<u8>, String> {
    let out = Command::new("wget")
        .args(["-qO-", "--timeout=30", url])
        .output()
        .map_err(|e| format!("wget not available ({e})"))?;
    if !out.status.success() {
        return Err(format!("wget exited with {}", out.status));
    }
    Ok(out.stdout)
}

// ---- list ------------------------------------------------------------------

fn cmd_list(args: &[String]) {
    let mut home = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--home" => home = it.next().cloned(),
            "--help" | "-h" => usage(),
            _ => usage(),
        }
    }
    let dirs = match home.as_deref() {
        Some(h) => vec![PathBuf::from(h)],
        None => runtime_search_dirs(),
    };
    let (found, resolvers) = installed_parts_all(&dirs);
    if found.is_empty() && resolvers.is_empty() {
        match home.as_deref() {
            Some(h) => println!("(no runtimes installed in {h})"),
            None => println!("(no runtimes installed)"),
        }
        return;
    }
    for (v, p) in found {
        println!("inka_runtime {v:<10} {}", p.display());
    }
    for (v, p) in resolvers {
        println!("inka_resolver {v:<10} {}", p.display());
    }
}

/// Scan a runtime dir for installed `libinka_runtime-*.so` / `libinka_resolver-*.so`
/// files, sorted by version. Reused by `inka list`, `inka doctor`, and `inka run`.
pub(crate) fn installed_parts(dir: &Path) -> (Vec<(Version, PathBuf)>, Vec<(Version, PathBuf)>) {
    let mut found: Vec<(Version, PathBuf)> = Vec::new();
    let mut resolvers: Vec<(Version, PathBuf)> = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            if let Some(stripped) = name.strip_prefix(FILENAME_PREFIX) {
                if let Some(vstr) = stripped.strip_suffix(FILENAME_SUFFIX) {
                    if let Some(v) = parse_version(vstr) {
                        found.push((v, ent.path()));
                    }
                }
            } else if let Some(stripped) = name.strip_prefix(RESOLVER_PREFIX) {
                if let Some(vstr) = stripped.strip_suffix(RESOLVER_SUFFIX) {
                    if let Some(v) = parse_version(vstr) {
                        resolvers.push((v, ent.path()));
                    }
                }
            }
        }
    }
    found.sort();
    resolvers.sort();
    (found, resolvers)
}

/// Merge installed parts across several runtime dirs (sorted by version).
pub(crate) fn installed_parts_all(
    dirs: &[PathBuf],
) -> (Vec<(Version, PathBuf)>, Vec<(Version, PathBuf)>) {
    let mut found: Vec<(Version, PathBuf)> = Vec::new();
    let mut resolvers: Vec<(Version, PathBuf)> = Vec::new();
    for d in dirs {
        let (f, r) = installed_parts(d);
        found.extend(f);
        resolvers.extend(r);
    }
    found.sort();
    resolvers.sort();
    (found, resolvers)
}

// ---- helpers ---------------------------------------------------------------

/// dlopen a resolver .so and read `inka_resolver_abi()` + `inka_resolver_version()`.
fn resolver_abi_etc(path: &Path) -> (i32, String) {
    let lib = match unsafe { libloading::Library::new(path) } {
        Ok(l) => l,
        Err(e) => return (-1, format!("load failed: {e}")),
    };
    let abi = unsafe {
        lib.get::<unsafe extern "C" fn() -> i32>(b"inka_resolver_abi")
            .map(|f| f())
            .unwrap_or(-1)
    };
    let version = unsafe {
        lib.get::<unsafe extern "C" fn() -> *const std::ffi::c_char>(b"inka_resolver_version")
            .ok()
            .and_then(|f| {
                let p = f();
                if p.is_null() {
                    None
                } else {
                    Some(std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned())
                }
            })
            .unwrap_or_default()
    };
    (abi, version)
}

/// Count package roots (dirs with package.json, one level deep; scope containers
/// count their children) under a node_modules-style pool root.
fn pool_package_count(pool: &Path) -> usize {
    let nm = pool.join("node_modules");
    let root = if nm.is_dir() { &nm } else { pool };
    let Ok(rd) = fs::read_dir(root) else {
        return 0;
    };
    let mut n = 0usize;
    for ent in rd.flatten() {
        let p = ent.path();
        let name = ent.file_name().to_string_lossy().into_owned();
        if p.join("package.json").is_file() {
            n += 1;
        } else if name.starts_with('@') {
            if let Ok(sub) = fs::read_dir(&p) {
                n += sub.flatten().filter(|s| s.path().join("package.json").is_file()).count();
            }
        }
    }
    n
}

fn seed_sha(store: &Path) -> String {
    fs::read(store.join("seed-manifest.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("sha256").and_then(serde_json::Value::as_str).map(str::to_string))
        .unwrap_or_default()
}

/// Generic read of a vendored.lock (never fails the report).
fn lock_summary(lock_path: &Path) -> (usize, Option<String>) {
    let Ok(raw) = fs::read_to_string(lock_path) else {
        return (0, None);
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return (0, None);
    };
    let entries = v
        .get("entries")
        .and_then(serde_json::Value::as_object)
        .map(|o| o.len())
        .unwrap_or(0);
    let store = v
        .get("store")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    (entries, store)
}

fn git_posture(vendored: &Path) -> String {
    let gi = vendored.parent().unwrap_or(vendored).join(".gitignore");
    match fs::read_to_string(&gi) {
        Ok(text) if text.lines().any(|l| l.trim().trim_end_matches('/') == "vendored") => {
            "ignore (dev)".to_string()
        }
        Ok(_) => "commit (release)".to_string(),
        Err(_) => "no .gitignore".to_string(),
    }
}

fn cmd_doctor(args: &[String]) {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!("usage: inka doctor");
        std::process::exit(0);
    }
    const EXPECTED_RESOLVER_ABI: i32 = 2;
    let mut warnings: Vec<String> = Vec::new();

    println!("[inka] doctor");
    let dirs = runtime_search_dirs();
    println!("runtime dirs:");
    for d in &dirs {
        println!("  {}", d.display());
    }
    let (runtimes, resolvers) = installed_parts_all(&dirs);
    if runtimes.is_empty() {
        println!("  runtimes: (none installed)");
        warnings.push("no runtimes installed; artifacts cannot run until `inka update`".into());
    }
    for (v, p) in &runtimes {
        println!("  runtime {v}  {}", p.display());
    }

    let (abi, res_version, res_path) = match resolvers.last() {
        Some((v, p)) => {
            let (a, s) = resolver_abi_etc(p);
            (Some(a), Some(s), Some((v.clone(), p.clone())))
        }
        None => (None, None, None),
    };
    match &res_path {
        None => {
            println!("  resolver: none installed (vendored resolution disabled)");
            warnings.push("no inka resolver installed; run `inka update` or set INKA_RESOLVER".into());
        }
        Some((v, p)) => {
            let abi = abi.unwrap_or(-1);
            println!("  resolver {v}  {}  abi={abi} version={}", p.display(), res_version.as_deref().unwrap_or("?"));
            if abi != EXPECTED_RESOLVER_ABI {
                warnings.push(format!(
                    "resolver abi {abi} != expected {EXPECTED_RESOLVER_ABI}; runtime/resolver mismatch"
                ));
            }
        }
    }

    let store = crate::vendor::store_dir();
    let store_present = store.join("node_modules").is_dir();
    let sha = seed_sha(&store);
    println!(
        "default store: {} ({}) packages={} sha={}",
        store.display(),
        if store_present { "present" } else { "absent" },
        pool_package_count(&store),
        if sha.is_empty() { "(none)" } else { &sha },
    );

    let vendored = crate::vendor::vendor_root();
    let vendored_count = pool_package_count(&vendored);
    println!("vendored pool (cwd): {vendored_count} package root(s)");
    if vendored_count > 0 {
        let posture = git_posture(&vendored);
        let ignored = posture == "ignore (dev)";
        println!(
            "git posture: {} (vendored/ {})",
            posture,
            if ignored { "ignored" } else { "not ignored" }
        );
    }

    let (lock_entries, lock_store) = lock_summary(&vendored.join("vendored.lock"));
    if lock_entries > 0 {
        println!("vendored.lock: {lock_entries} entr{}", if lock_entries == 1 { "y" } else { "ies" });
        let current = format!(
            "{} sha256={}",
            store.display(),
            if sha.is_empty() { "no-sha-record" } else { &sha }
        );
        match lock_store {
            Some(recorded) if recorded != current => {
                if store_present {
                    warnings.push(format!(
                        "vendored set was built against a different default store ({recorded}); reseed or vendor the affected deps"
                    ));
                }
            }
            _ => {}
        }
        if !store_present {
            warnings.push("default store is missing but vendored.lock records deps it would provide; reseed or vendor the affected deps".into());
        }
    }

    if warnings.is_empty() {
        println!("warnings: none");
    } else {
        println!("warnings:");
        for w in &warnings {
            println!("  - {w}");
        }
    }
}


pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn os(s: &str) -> &OsStr {
        OsStr::new(s)
    }

    #[test]
    fn data_root_prefers_absolute_xdg() {
        assert_eq!(
            data_root(Some(os("/home/u")), Some(os("/xdg"))),
            PathBuf::from("/xdg")
        );
    }

    #[test]
    fn data_root_ignores_relative_or_empty_xdg() {
        assert_eq!(
            data_root(Some(os("/home/u")), Some(os("relative"))),
            PathBuf::from("/home/u/.local/share")
        );
        assert_eq!(
            data_root(Some(os("/home/u")), Some(os(""))),
            PathBuf::from("/home/u/.local/share")
        );
    }

    #[test]
    fn data_root_falls_back_to_home_then_dot() {
        assert_eq!(
            data_root(Some(os("/home/u")), None),
            PathBuf::from("/home/u/.local/share")
        );
        assert_eq!(data_root(None, None), PathBuf::from("."));
        assert_eq!(data_root(Some(os("")), None), PathBuf::from("."));
    }

    #[test]
    fn xdg_store_and_runtime_are_siblings_under_inka() {
        // Derived from data_root; assert the shape without touching the env.
        let root = data_root(Some(os("/home/u")), None);
        assert_eq!(root.join("inka/store"), PathBuf::from("/home/u/.local/share/inka/store"));
        assert_eq!(root.join("inka/runtime"), PathBuf::from("/home/u/.local/share/inka/runtime"));
    }
}

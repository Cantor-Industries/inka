// inka: companion tooling for inka artifacts.
//
//   inka build [source] [-s|--source <file>] [-o|--output <file>] [--manifest <file>]
//   inka install <version> [--from <dir-or-url>] [--sha256 <hex>]
//                          [--insecure] [--home <dir>]
//   inka list [--home <dir>]

mod build;
mod config;
mod embed;
mod pkg;
mod run;
mod transpile;
mod vendor;

use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

const FILENAME_PREFIX: &str = "libinka_runtime-";
const FILENAME_SUFFIX: &str = ".so";
const RESOLVER_PREFIX: &str = "libinka_resolver-";
const RESOLVER_SUFFIX: &str = ".so";
/// Resolver version used when fetching from a URL base that has no directory
/// listing (and $INKA_RESOLVER_VERSION is unset).
const DEFAULT_RESOLVER_VERSION: &str = "1.0.0";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Version(u64, u64, u64);

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
        "usage:\n  inka build [source] [-s|--source <file>] [-o|--output <file>] [--manifest <file>]\n  inka install <version> [--from <dir-or-url>] [--sha256 <hex>] [--insecure] [--home <dir>]\n  inka list [--home <dir>]\n  inka add <pkg[@ver]>        vendor a package not in the default store\n  inka remove <pkg>           un-vendor a package (+ prune orphaned vendored deps)\n  inka vendor list|status|release|ignore\n  inka pkg snapshot|seed|list (default-store snapshot; see `inka pkg --help`)\n  inka doctor                 print a diagnostic report (runtimes, resolver, store, vendored)\n  inka run [-A] [-P[=name]] [--allow-<cat>[=list]|--deny-<cat>[=list]]... <file> [args...]\n                             execute a ts/js file via the installed runtime"
    );
    std::process::exit(2);
}

pub(crate) fn runtime_dir(home_override: Option<&str>) -> PathBuf {
    if let Some(h) = home_override {
        return PathBuf::from(h);
    }
    if let Ok(h) = env::var("INKA_RUNTIME_HOME") {
        return PathBuf::from(h);
    }
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| ".".into())).join(".inka-runtime")
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }
    match args[0].as_str() {
        "build" => build::cmd_build(&args[1..]),
        "install" => cmd_install(&args[1..]),
        "list" => cmd_list(&args[1..]),
        "add" => vendor::cmd_add(&args[1..]),
        "remove" => vendor::cmd_remove(&args[1..]),
        "vendor" => vendor::cmd_vendor(&args[1..]),
        "pkg" => pkg::cmd_pkg(&args[1..]),
        "doctor" => cmd_doctor(&args[1..]),
        "run" => run::cmd_run(&args[1..]),
        _ => usage(),
    }
}

// ---- install ---------------------------------------------------------------

fn cmd_install(args: &[String]) {
    let mut version = None;
    let mut from = None;
    let mut sha256 = None;
    let mut insecure = false;
    let mut home = None;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--from" => from = it.next().cloned(),
            "--sha256" => sha256 = it.next().cloned(),
            "--home" => home = it.next().cloned(),
            "--insecure" => insecure = true,
            "--help" | "-h" => usage(),
            other => {
                if version.is_none() {
                    version = Some(other.to_string());
                } else {
                    usage();
                }
            }
        }
    }

    let Some(version_str) = version else { usage() };
    let ver = parse_version(&version_str).unwrap_or_else(|| {
        eprintln!("error: '{version_str}' is not a valid x.y.z version");
        std::process::exit(2);
    });
    let file_name = format!("{FILENAME_PREFIX}{ver}{FILENAME_SUFFIX}");

    let source = from.or_else(|| env::var("INKA_RT_SOURCE").ok());
    let Some(source) = source else {
        eprintln!("error: no runtime source given (use --from <dir-or-url> or INKA_RT_SOURCE)");
        std::process::exit(2);
    };

    let target_dir = runtime_dir(home.as_deref());
    fs::create_dir_all(&target_dir).unwrap_or_else(|e| {
        eprintln!("error: cannot create {}: {e}", target_dir.display());
        std::process::exit(1);
    });

    println!("[inka] installing inka_runtime {ver} from {source}");

    let (bytes, sidecar_sha) = match fetch_with_sidecar(&source, &file_name) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("error: failed to fetch {file_name} from {source}: {e}");
            std::process::exit(1);
        }
    };

    let expected: Option<String> = match (sha256.clone(), sidecar_sha) {
        (Some(h), _) => Some(h),
        (None, Some(h)) => Some(h),
        (None, None) if insecure => None,
        (None, None) => {
            eprintln!("error: no checksum available for {file_name}");
            eprintln!("  provide --sha256 <hex>, publish a {file_name}.sha256 sidecar,");
            eprintln!("  or pass --insecure to skip verification");
            std::process::exit(1);
        }
    };

    // sha256sum-style sidecars look like "<hex>  <filename>"; accept a bare hex too.
    let expected = expected.map(|e| {
        e.split_whitespace()
            .next()
            .unwrap_or(&e)
            .trim()
            .to_ascii_lowercase()
    });

    let actual = hex(&Sha256::digest(&bytes));
    if let Some(exp) = expected {
        if exp != actual {
            eprintln!("error: checksum mismatch for {file_name}");
            eprintln!("  expected {exp}");
            eprintln!("  actual   {actual}");
            std::process::exit(1);
        }
        println!("[inka] checksum ok ({})", &actual[..12]);
    } else {
        println!("[inka] checksum skipped (--insecure)  sha256={actual}");
    }

    let target = target_dir.join(&file_name);
    install_atomically(&target, &bytes);

    println!(
        "[inka] installed {} ({})",
        target.display(),
        bytes.len()
    );

    // Ship the runtime release's vendored store payload (if any) into the store
    // next to the runtime home, so artifacts can load curated packages at once.
    let store_target = match env::var_os("INKA_STORE") {
        Some(s) => PathBuf::from(s),
        None => runtime_dir(home.as_deref()).join("store"),
    };
    match pkg::seed_release_store(&source, &store_target) {
        Ok(Some(n)) if n > 0 => {
            println!(
                "[inka] installed {n} store package(s) into {}",
                store_target.display()
            );
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!("error: store payload: {e}");
            std::process::exit(1);
        }
    }

    install_resolver_payload(&source, &target_dir, insecure);
}

fn install_resolver_payload(base: &str, target_dir: &Path, insecure: bool) {
    // Pick a resolver from the release: newest libinka_resolver-*.so in a local
    // dir, else a URL fetch of the current resolver version ($INKA_RESOLVER_VERSION
    // overrides; DEFAULT_RESOLVER_VERSION fallback).
    let name = if Path::new(base).is_dir() {
        let mut best: Option<(Version, String)> = None;
        if let Ok(rd) = fs::read_dir(base) {
            for ent in rd.flatten() {
                let n = ent.file_name().to_string_lossy().into_owned();
                let Some(stripped) = n.strip_prefix(RESOLVER_PREFIX) else { continue };
                let Some(vstr) = stripped.strip_suffix(RESOLVER_SUFFIX) else { continue };
                if let Some(v) = parse_version(vstr) {
                    if best.as_ref().map_or(true, |(bv, _)| v > *bv) {
                        best = Some((v, n));
                    }
                }
            }
        }
        best.map(|(_, n)| n)
    } else {
        let ver = env::var("INKA_RESOLVER_VERSION")
            .unwrap_or_else(|_| DEFAULT_RESOLVER_VERSION.to_string());
        Some(format!("{RESOLVER_PREFIX}{ver}{RESOLVER_SUFFIX}"))
    };
    let Some(name) = name else {
        return; // release ships no resolver
    };

    let (bytes, sidecar_sha) = match fetch_with_sidecar(base, &name) {
        Ok(x) => x,
        Err(_) => return, // not present on this source
    };
    let expected = sidecar_sha.and_then(|s| {
        s.split_whitespace()
            .next()
            .map(|x| x.trim().to_ascii_lowercase())
    });
    let actual = hex(&Sha256::digest(&bytes));
    match (&expected, insecure) {
        (Some(exp), _) if exp != &actual => {
            eprintln!("error: checksum mismatch for {name}");
            eprintln!("  expected {exp}");
            eprintln!("  actual   {actual}");
            std::process::exit(1);
        }
        (Some(_), _) => {}
        (None, false) => {
            eprintln!("error: no checksum available for {name}");
            eprintln!("  publish a {name}.sha256 sidecar, or pass --insecure to trust it");
            std::process::exit(1);
        }
        (None, true) => {}
    }
    let target = target_dir.join(&name);
    install_atomically(&target, &bytes);
    println!(
        "[inka] installed resolver {} ({})",
        target.display(),
        bytes.len()
    );
}

fn install_atomically(target: &Path, bytes: &[u8]) {
    let tmp = target.with_extension(format!("so.tmp{}", std::process::id()));
    fs::write(&tmp, bytes).unwrap_or_else(|e| {
        eprintln!("error: cannot write {}: {e}", tmp.display());
        std::process::exit(1);
    });
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap_or_else(|e| {
        eprintln!("error: cannot chmod {}: {e}", tmp.display());
        let _ = fs::remove_file(&tmp);
        std::process::exit(1);
    });
    fs::rename(&tmp, target).unwrap_or_else(|e| {
        eprintln!("error: cannot move {} into place: {e}", target.display());
        let _ = fs::remove_file(&tmp);
        std::process::exit(1);
    });
}

use std::os::unix::fs::PermissionsExt;

/// Fetch `<base>/<file>` plus `<base>/<file>.sha256` when available.
/// `base` may be a local directory path or an http(s) URL.
pub(crate) fn fetch_with_sidecar(base: &str, file: &str) -> Result<(Vec<u8>, Option<String>), String> {
    let is_url = base.starts_with("http://") || base.starts_with("https://");
    let main = fetch_one(base, file, is_url)?;
    let sidecar = fetch_optional(base, &format!("{file}.sha256"), is_url)?;
    Ok((main, sidecar))
}

fn fetch_one(base: &str, file: &str, is_url: bool) -> Result<Vec<u8>, String> {
    if is_url {
        fetch_http(&format!("{}/{}", base.trim_end_matches('/'), file))
    } else {
        let p = PathBuf::from(base).join(file);
        fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))
    }
}

fn fetch_optional(base: &str, file: &str, is_url: bool) -> Result<Option<String>, String> {
    match fetch_one(base, file, is_url) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(_) => Ok(None),
    }
}

fn fetch_http(url: &str) -> Result<Vec<u8>, String> {
    let out = Command::new("curl")
        .args(["-fsSL", url])
        .output()
        .map_err(|e| format!("failed to spawn curl ({e}); HTTP sources need curl installed"))?;
    if !out.status.success() {
        return Err(format!("curl exited with {}", out.status));
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
    let dir = runtime_dir(home.as_deref());
    let (found, resolvers) = installed_parts(&dir);
    if !dir.is_dir() {
        println!("(no runtimes installed in {})", dir.display());
        return;
    }
    if found.is_empty() && resolvers.is_empty() {
        println!("(no runtimes installed in {})", dir.display());
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
    let dir = runtime_dir(None);
    println!("runtime dir: {}", dir.display());
    let (runtimes, resolvers) = installed_parts(&dir);
    if runtimes.is_empty() {
        println!("  runtimes: (none installed)");
        warnings.push("no runtimes installed; artifacts cannot run until `inka install <version>`".into());
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
            warnings.push("no inka resolver installed; install one with `inka install` or set INKA_RESOLVER".into());
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

// dex: companion tooling for dex artifacts.
//
//   dex build [source] [-s|--source <file>] [-o|--output <file>] [--manifest <file>]
//   dex install <version> [--from <dir-or-url>] [--sha256 <hex>]
//                         [--insecure] [--home <dir>]
//   dex list [--home <dir>]

mod build;
mod embed;
mod transpile;

use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

const FILENAME_PREFIX: &str = "libdeno_runtime-";
const FILENAME_SUFFIX: &str = ".so";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Version(u64, u64, u64);

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

fn parse_version(s: &str) -> Option<Version> {
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
        "usage:\n  dex build [source] [-s|--source <file>] [-o|--output <file>] [--manifest <file>]\n  dex install <version> [--from <dir-or-url>] [--sha256 <hex>] [--insecure] [--home <dir>]\n  dex list [--home <dir>]"
    );
    std::process::exit(2);
}

fn runtime_dir(home_override: Option<&str>) -> PathBuf {
    if let Some(h) = home_override {
        return PathBuf::from(h);
    }
    if let Ok(h) = env::var("DENO_RUNTIME_HOME") {
        return PathBuf::from(h);
    }
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| ".".into())).join(".deno-runtime")
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

    let source = from.or_else(|| env::var("DEX_RT_SOURCE").ok());
    let Some(source) = source else {
        eprintln!("error: no runtime source given (use --from <dir-or-url> or DEX_RT_SOURCE)");
        std::process::exit(2);
    };

    let target_dir = runtime_dir(home.as_deref());
    fs::create_dir_all(&target_dir).unwrap_or_else(|e| {
        eprintln!("error: cannot create {}: {e}", target_dir.display());
        std::process::exit(1);
    });

    println!("[dex] installing deno_runtime {ver} from {source}");

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
        println!("[dex] checksum ok ({})", &actual[..12]);
    } else {
        println!("[dex] checksum skipped (--insecure)  sha256={actual}");
    }

    let target = target_dir.join(&file_name);
    install_atomically(&target, &bytes);

    println!(
        "[dex] installed {} ({})",
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
fn fetch_with_sidecar(base: &str, file: &str) -> Result<(Vec<u8>, Option<String>), String> {
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
    if !dir.is_dir() {
        println!("(no runtimes installed in {})", dir.display());
        return;
    }
    let mut found: Vec<(Version, PathBuf)> = Vec::new();
    for ent in fs::read_dir(&dir).unwrap().flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        let Some(stripped) = name.strip_prefix(FILENAME_PREFIX) else {
            continue;
        };
        let Some(vstr) = stripped.strip_suffix(FILENAME_SUFFIX) else {
            continue;
        };
        if let Some(v) = parse_version(vstr) {
            found.push((v, ent.path()));
        }
    }
    found.sort();
    if found.is_empty() {
        println!("(no runtimes installed in {})", dir.display());
        return;
    }
    for (v, p) in found {
        println!("deno_runtime {v:<10} {}", p.display());
    }
}

// ---- helpers ---------------------------------------------------------------

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

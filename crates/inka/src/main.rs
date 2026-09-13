// inka: companion tooling for inka artifacts.
//
//   inka build [source] [-s|--source <file>] [-o|--output <file>]
//               [--runtime <spec>] [--tested-against <ver>] [-P <name>]
//               [--transpile] [--embed-dir]
//   inka update [<version>] [--from <dir-or-url>] [--sha256 <hex>]
//                           [--insecure] [--home <dir>]
//   inka list [--home <dir>]

mod build;
mod config;
mod embed;
mod run;
mod update;

use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) const FILENAME_PREFIX: &str = "libinka_runtime-";
pub(crate) const FILENAME_SUFFIX: &str = ".so";

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
        "usage:\n  inka build [source] [-s|--source <file>] [-o|--output <file>] [--runtime <spec>] [--tested-against <ver>] [-P <name>] [--transpile] [--embed-dir]\n  inka update [<version>] [--from <dir-or-url>] [--sha256 <hex>] [--insecure] [--home <dir>]\n  inka list [--home <dir>]\n  inka doctor                 print a diagnostic report (runtimes)\n  inka run [-A] [-P[=name]] [--allow-<cat>[=list]|--deny-<cat>[=list]]... <file> [args...]\n                             execute a ts/js file via the installed runtime\n  inka --version, -V          print the inka toolchain version"
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
    data_root(
        env::var_os("HOME").as_deref(),
        env::var_os("XDG_DATA_HOME").as_deref(),
    )
}

/// Per-user inka data dir: `$XDG_DATA_HOME/inka` (`~/.local/share/inka`).
pub(crate) fn inka_data_dir() -> PathBuf {
    data_root_now().join("inka")
}

/// Per-user runtime dir: `<data>/inka/runtime`.
pub(crate) fn user_runtime_dir() -> PathBuf {
    inka_data_dir().join("runtime")
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

/// Directories searched for installed runtime `.so` files, in order:
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
        "doctor" => cmd_doctor(&args[1..]),
        "run" => run::cmd_run(&args[1..]),
        _ => usage(),
    }
}

// ---- fetch helpers ---------------------------------------------------------

/// Fetch `<base>/<file>` plus `<base>/<file>.sha256` when available.
/// `base` may be a local directory path or an http(s) URL.
pub(crate) fn fetch_with_sidecar(
    base: &str,
    file: &str,
) -> Result<(Vec<u8>, Option<String>), String> {
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
        Ok(_) => {}                                                  // try wget
        Err(_) => {}                                                 // curl missing
    }
    match Command::new("wget")
        .args(["-qO-", "--timeout=30", url])
        .output()
    {
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
    let found = installed_parts_all(&dirs);
    if found.is_empty() {
        match home.as_deref() {
            Some(h) => println!("(no runtimes installed in {h})"),
            None => println!("(no runtimes installed)"),
        }
        return;
    }
    for (v, p) in found {
        println!("inka_runtime {v:<10} {}", p.display());
    }
}

/// Scan a runtime dir for installed `libinka_runtime-*.so` files, sorted by
/// version. Reused by `inka list`, `inka doctor`, and `inka run`.
pub(crate) fn installed_parts(dir: &Path) -> Vec<(Version, PathBuf)> {
    let mut found: Vec<(Version, PathBuf)> = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            if let Some(stripped) = name.strip_prefix(FILENAME_PREFIX) {
                if let Some(vstr) = stripped.strip_suffix(FILENAME_SUFFIX) {
                    if let Some(v) = parse_version(vstr) {
                        found.push((v, ent.path()));
                    }
                }
            }
        }
    }
    found.sort();
    found
}

/// Merge installed runtimes across several runtime dirs (sorted by version).
pub(crate) fn installed_parts_all(dirs: &[PathBuf]) -> Vec<(Version, PathBuf)> {
    let mut found: Vec<(Version, PathBuf)> = Vec::new();
    for d in dirs {
        found.extend(installed_parts(d));
    }
    found.sort();
    found
}

fn cmd_doctor(args: &[String]) {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!("usage: inka doctor");
        std::process::exit(0);
    }
    let mut warnings: Vec<String> = Vec::new();

    println!("[inka] doctor");
    let dirs = runtime_search_dirs();
    println!("runtime dirs:");
    for d in &dirs {
        println!("  {}", d.display());
    }
    let runtimes = installed_parts_all(&dirs);
    if runtimes.is_empty() {
        println!("  runtimes: (none installed)");
        warnings.push("no runtimes installed; artifacts cannot run until `inka update`".into());
    }
    for (v, p) in &runtimes {
        println!("  runtime {v}  {}", p.display());
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
    fn xdg_runtime_is_under_inka() {
        // Derived from data_root; assert the shape without touching the env.
        let root = data_root(Some(os("/home/u")), None);
        assert_eq!(
            root.join("inka/runtime"),
            PathBuf::from("/home/u/.local/share/inka/runtime")
        );
    }
}

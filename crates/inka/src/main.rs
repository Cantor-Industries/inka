// inka: companion tooling for inka artifacts.
//
//   inka build [source] [-s|--source <file>] [-o|--output <file>]
//               [--runtime <spec>] [--tested-against <ver>] [-P[=<set>]]
//               [--minify] [--sourcemap] [--external <pkg>]... [--embed-dir]
//   inka run [-A] [-P[=name]] [--allow-<cat>[=list]]... <file> [args...]
//   inka update [<version>] [--from <dir-or-url>] [--sha256 <hex>]
//                           [--insecure] [--home <dir>]
//   inka doctor
//   inka help [command]

mod build;
mod channel;
mod config;
mod embed;
mod help;
mod permissions;
mod run;
mod ui;
mod update;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use deno_terminal::colors;
use inka_format::{archive_index, constraint_allows, parse_manifest, read_layout};
use sha2::{Digest, Sha256};

pub(crate) const FILENAME_PREFIX: &str = "libinka_runtime-";
pub(crate) const FILENAME_SUFFIX: &str = ".so";

/// Version parsing/rendering lives in `inka-format`, shared with the launcher.
pub(crate) use inka_format::{parse_version, Version};

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

/// Top-level commands, in help order and for suggestions.
pub(crate) const COMMANDS: [&str; 6] = ["build", "run", "cache", "update", "doctor", "help"];

fn main() {
    // Restore the default SIGPIPE disposition: piping output into `head`/`grep -q`
    // closes the pipe, and the default action (terminate quietly) is preferable to
    // Rust's panic-on-EPIPE, which aborts with `panic = "abort"`.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    ui::init_from_env();
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(first) = args.first() else {
        help::print(help::top(), help::Mode::Long);
        return;
    };
    match first.as_str() {
        "-h" => help::print(help::top(), help::Mode::Short),
        "--help" => help::print(help::top(), help::Mode::Long),
        "--version" | "-V" => println!("inka {}", channel::release_version()),
        "help" => cmd_help(&args[1..]),
        "build" => build::cmd_build(&args[1..]),
        "run" => run::cmd_run(&args[1..]),
        "cache" => cmd_cache(&args[1..]),
        "update" => update::cmd_update(&args[1..]),
        "doctor" => cmd_doctor(&args[1..]),
        other if other.starts_with('-') => {
            ui::log_error(format!("unknown option '{other}'"));
            ui::hint("run `inka --help` for usage");
            std::process::exit(2);
        }
        other => unknown_command(other),
    }
}

/// Report an unknown command and exit 2 (with a suggestion when close).
fn unknown_command(name: &str) -> ! {
    ui::log_error(format!("unrecognized command '{name}'"));
    match help::suggest(name, &COMMANDS) {
        Some(s) => ui::hint(format!("did you mean `inka {s}`?")),
        None => ui::hint("run `inka --help` for usage"),
    }
    std::process::exit(2);
}

fn cmd_help(args: &[String]) {
    match args.first().map(String::as_str) {
        None => help::print(help::top(), help::Mode::Long),
        Some("-h") => help::print(help::top(), help::Mode::Short),
        Some("--help") => help::print(help::top(), help::Mode::Long),
        Some("build") => help::print(help::build(), help::Mode::Long),
        Some("run") => help::print(help::run(), help::Mode::Long),
        Some("cache") => help::print(help::cache(), help::Mode::Long),
        Some("update") => help::print(help::update(), help::Mode::Long),
        Some("doctor") => help::print(help::doctor(), help::Mode::Long),
        Some("help") => help::print(help::help(), help::Mode::Long),
        Some(other) => unknown_command(other),
    }
}

// ---- fetch helpers ---------------------------------------------------------

/// Fetch `<base>/<file>.sha256` when available. `base` may be a local directory
/// path or an http(s) URL.
pub(crate) fn fetch_sidecar(base: &str, file: &str) -> Result<Option<String>, String> {
    let is_url = base.starts_with("http://") || base.starts_with("https://");
    fetch_optional(base, &format!("{file}.sha256"), is_url)
}

/// Lowercase hex sha256 of a file on disk (streamed; never buffers it whole).
pub(crate) fn sha256_file(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut f = fs::File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

/// Fetch `<base>/<file>` as UTF-8 text (for release metadata like versions.json).
pub(crate) fn fetch_text(base: &str, file: &str) -> Result<String, String> {
    let is_url = base.starts_with("http://") || base.starts_with("https://");
    let bytes = fetch_one(base, file, is_url)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Fetch an absolute URL as UTF-8 text (e.g. the GitHub Releases API).
pub(crate) fn fetch_url(url: &str) -> Result<String, String> {
    let bytes = fetch_http(url)?;
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
        // Pin redirect protocols too: `--proto` alone does not guarantee it.
        cmd.args(["--proto-redir", "=https", "--max-redirs", "5"]);
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
    let mut wcmd = Command::new("wget");
    if url.starts_with("https://") {
        wcmd.arg("--https-only");
    }
    match wcmd.args(["-qO-", "--timeout=30", url]).output() {
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
pub(crate) fn fetch_http(url: &str) -> Result<Vec<u8>, String> {
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

/// An optional token for api.github.com (raises the unauthenticated 60/hr rate
/// limit). Read from `INKA_GITHUB_TOKEN` then `GITHUB_TOKEN`.
fn github_token() -> Option<String> {
    env::var("INKA_GITHUB_TOKEN")
        .ok()
        .or_else(|| env::var("GITHUB_TOKEN").ok())
        .filter(|s| !s.is_empty())
}

/// Add an `Authorization` header for GitHub API requests when a token is set.
/// Only applied to `api.github.com`, so the token is never sent elsewhere.
fn apply_github_auth(cmd: &mut Command, url: &str) {
    if url.contains("api.github.com") {
        if let Some(tok) = github_token() {
            cmd.arg("-H").arg(format!("Authorization: Bearer {tok}"));
        }
    }
}

fn curl_get(url: &str) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new("curl");
    // Pin TLS for https (avoid downgrade); allow plain http for local mirrors.
    if url.starts_with("https://") {
        cmd.args(["--proto", "=https", "--tlsv1.2"]);
        // `--proto` does not necessarily cover redirects; pin those explicitly.
        cmd.args(["--proto-redir", "=https", "--max-redirs", "5"]);
    }
    apply_github_auth(&mut cmd, url);
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
    let mut cmd = Command::new("wget");
    if url.starts_with("https://") {
        cmd.arg("--https-only");
    }
    if url.contains("api.github.com") {
        if let Some(tok) = github_token() {
            cmd.arg("--header")
                .arg(format!("Authorization: Bearer {tok}"));
        }
    }
    let out = cmd
        .args(["-qO-", "--timeout=30", url])
        .output()
        .map_err(|e| format!("wget not available ({e})"))?;
    if !out.status.success() {
        return Err(format!("wget exited with {}", out.status));
    }
    Ok(out.stdout)
}

// ---- file downloads (progress) ---------------------------------------------

/// Download `<base>/<file>` to `dest`, drawing a progress bar on a TTY.
/// `base` may be a local directory (a plain copy) or an http(s) URL.
pub(crate) fn download_file(base: &str, file: &str, dest: &Path) -> Result<(), String> {
    let is_url = base.starts_with("http://") || base.starts_with("https://");
    if !is_url {
        let src = PathBuf::from(base).join(file);
        fs::copy(&src, dest).map_err(|e| format!("{}: {e}", src.display()))?;
        return Ok(());
    }
    let url = format!("{}/{}", base.trim_end_matches('/'), file);
    let total = url_content_length(&url);

    let _ = fs::remove_file(dest);
    let curl_result = match spawn_curl_download(&url, dest) {
        Ok(child) => drive_download(child, file, dest, total),
        Err(e) => Err(e),
    };
    match curl_result {
        Ok(()) => Ok(()),
        Err(curl_err) => {
            let _ = fs::remove_file(dest);
            match spawn_wget_download(&url, dest) {
                Ok(child) => match drive_download(child, file, dest, total) {
                    Ok(()) => Ok(()),
                    Err(wget_err) => Err(format!(
                        "failed to download {url}\n  curl: {curl_err}\n  wget: {wget_err}"
                    )),
                },
                Err(e) => Err(format!(
                    "failed to download {url}\n  curl: {curl_err}\n  wget: {e}"
                )),
            }
        }
    }
}

/// Spawn curl writing `url` to `dest` (silenced but for errors).
fn spawn_curl_download(url: &str, dest: &Path) -> Result<std::process::Child, String> {
    let mut cmd = Command::new("curl");
    if url.starts_with("https://") {
        cmd.args(["--proto", "=https", "--tlsv1.2"]);
        cmd.args(["--proto-redir", "=https", "--max-redirs", "5"]);
    }
    cmd.args(["-fsSL", "--connect-timeout", "30", "--max-time", "1800"])
        .arg("-o")
        .arg(dest)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("curl not available ({e})"))
}

/// Spawn wget writing `url` to `dest` (silenced but for errors).
fn spawn_wget_download(url: &str, dest: &Path) -> Result<std::process::Child, String> {
    let mut cmd = Command::new("wget");
    if url.starts_with("https://") {
        cmd.arg("--https-only");
    }
    cmd.args(["-qO"])
        .arg(dest)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("wget not available ({e})"))
}

/// Poll a running download's output file while drawing the progress bar, then
/// wait and surface the engine's stderr on failure.
fn drive_download(
    mut child: std::process::Child,
    file: &str,
    dest: &Path,
    total: Option<u64>,
) -> Result<(), String> {
    let mut progress = ui::Progress::download(file, total);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(e) => return Err(format!("{e}")),
        }
        match fs::metadata(dest) {
            Ok(md) => progress.set(md.len()),
            Err(_) => progress.tick(),
        }
        std::thread::sleep(Duration::from_millis(80));
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    progress.finish();
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stderr = stderr.trim();
        Err(if stderr.is_empty() {
            format!("exit {}", out.status)
        } else {
            stderr.to_string()
        })
    }
}

/// Best-effort `Content-Length` of a URL (follows redirects; takes the final
/// `200`). `None` when HEAD is unavailable or the size is unknown.
fn url_content_length(url: &str) -> Option<u64> {
    let mut cmd = Command::new("curl");
    if url.starts_with("https://") {
        cmd.args(["--proto", "=https", "--tlsv1.2"]);
        cmd.args(["--proto-redir", "=https", "--max-redirs", "5"]);
    }
    let out = cmd
        .args(["-fsSLI", "--connect-timeout", "30", "--max-time", "60", url])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().rev().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if !key.trim().eq_ignore_ascii_case("content-length") {
            return None;
        }
        value.trim().parse::<u64>().ok()
    })
}

// ---- installed runtimes ----------------------------------------------------

/// Scan a runtime dir for installed `libinka_runtime-*.so` files, sorted by
/// version. Reused by `inka doctor` and `inka run`.
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

/// Effective Deno cache dir from `$DENO_DIR` (non-empty) else `~/.cache/deno`.
fn deno_dir_root(env_deno: Option<&std::ffi::OsStr>, home: Option<&std::ffi::OsStr>) -> PathBuf {
    if let Some(d) = env_deno {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    let home = home
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| std::ffi::OsStr::new("."));
    PathBuf::from(home).join(".cache/deno")
}

pub(crate) fn default_deno_dir() -> PathBuf {
    deno_dir_root(
        env::var_os("DENO_DIR").as_deref(),
        env::var_os("HOME").as_deref(),
    )
}

/// Path to the `inka-launcher` binary (`$INKA_LAUNCHER`, else next to the
/// running `inka`), when present.
fn launcher_path() -> Option<PathBuf> {
    if let Ok(p) = env::var("INKA_LAUNCHER") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            let adjacent = dir.join("inka-launcher");
            if adjacent.is_file() {
                return Some(adjacent);
            }
        }
    }
    None
}

/// `inka cache <file>`: fetch missing remote (`jsr:`/`https:`) modules into the
/// Deno cache so later (offline) builds/runs resolve them. Opt-in network.
#[cfg(feature = "bundle")]
fn cmd_cache(args: &[String]) {
    let mut positional: Vec<&str> = Vec::new();
    for a in args {
        match a.as_str() {
            "-h" => {
                help::print(help::cache(), help::Mode::Short);
                std::process::exit(0);
            }
            "--help" => {
                help::print(help::cache(), help::Mode::Long);
                std::process::exit(0);
            }
            other if ui::apply_verbosity_flag(other) => {}
            other if other.starts_with('-') => {
                ui::log_error(format!("unknown option '{other}'"));
                ui::hint("run `inka cache --help` for usage");
                std::process::exit(2);
            }
            other => positional.push(other),
        }
    }
    let file = match positional.as_slice() {
        [f] => PathBuf::from(f),
        [] => {
            ui::log_error("no file given");
            ui::hint("run `inka cache --help` for usage");
            std::process::exit(2);
        }
        _ => {
            ui::log_error("inka cache takes one entry file");
            std::process::exit(2);
        }
    };
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let entry = if file.is_absolute() {
        file.clone()
    } else {
        cwd.join(&file)
    };
    if !entry.is_file() {
        ui::log_error(format!("source file not found: {}", file.display()));
        std::process::exit(1);
    }
    ui::title("cache");
    ui::section("Fetch");
    ui::row("entry", file.display());
    match inka_bundler::warm_cache(&cwd, &entry) {
        Ok(n) => {
            ui::ok("fetched", format!("{n} remote module(s)"));
            ui::status_ok("cache warmed");
        }
        Err(e) => {
            ui::log_error(e);
            std::process::exit(1);
        }
    }
}

#[cfg(not(feature = "bundle"))]
fn cmd_cache(args: &[String]) {
    for a in args {
        if a == "-h" || a == "--help" {
            help::print(help::cache(), help::Mode::Short);
            std::process::exit(0);
        }
    }
    ui::log_error("inka was built without fetching support (rebuild with `--features bundle`)");
    std::process::exit(1);
}

fn cmd_doctor(args: &[String]) {
    let mut positional: Vec<&str> = Vec::new();
    let mut json = false;
    let mut beta = false;
    let mut stable = false;
    for a in args {
        match a.as_str() {
            "-h" => {
                help::print(help::doctor(), help::Mode::Short);
                std::process::exit(0);
            }
            "--help" => {
                help::print(help::doctor(), help::Mode::Long);
                std::process::exit(0);
            }
            "--json" => json = true,
            "--beta" => beta = true,
            "--stable" => stable = true,
            other if ui::apply_verbosity_flag(other) => {}
            other if other.starts_with('-') => {
                ui::log_error(format!("unknown option '{other}'"));
                ui::hint("run `inka doctor --help` for usage");
                std::process::exit(2);
            }
            other => positional.push(other),
        }
    }
    let requested = match channel::flag_request(beta, stable) {
        Ok(r) => r,
        Err(e) => {
            ui::log_error(e);
            std::process::exit(2);
        }
    };
    let effective = channel::resolve_or_exit(requested);
    match positional.as_slice() {
        [] => doctor_machine(effective),
        [artifact] => doctor_artifact(Path::new(artifact), json, effective),
        _ => {
            ui::log_error("doctor takes at most one artifact path");
            ui::hint("run `inka doctor --help` for usage");
            std::process::exit(2);
        }
    }
}

/// Why an artifact could not be inspected.
#[derive(Debug)]
enum ArtifactError {
    /// No `INKFOOT5` trailer (the file is not an inka executable).
    NotArtifact(String),
    /// An inka trailer whose archive/manifest is malformed or unparseable.
    Malformed(String),
}

/// Parsed artifact contents for inspection.
struct ArtifactInfo {
    manifest: inka_format::Manifest,
    entries: Vec<(String, usize)>,
    size: usize,
}

/// Parse a full artifact image into its manifest and payload index (pure, so it
/// is testable without touching the filesystem or exiting).
fn parse_artifact(bytes: &[u8]) -> Result<ArtifactInfo, ArtifactError> {
    let layout = read_layout(bytes).map_err(ArtifactError::NotArtifact)?;
    let manifest =
        parse_manifest(&bytes[layout.manifest_off..layout.manifest_off + layout.manifest_len]);
    let entries =
        archive_index(&bytes[layout.archive_off..layout.archive_off + layout.archive_len])
            .map_err(ArtifactError::Malformed)?;
    if let Some(bad) = &manifest.malformed {
        return Err(ArtifactError::Malformed(format!(
            "unparseable version constraint: {bad}"
        )));
    }
    Ok(ArtifactInfo {
        manifest,
        entries,
        size: bytes.len(),
    })
}

/// The newest installed runtime satisfying a manifest's constraints. Prerelease
/// tuples are only eligible when `allow_prerelease` (beta channel) is set.
fn select_runtime(
    m: &inka_format::Manifest,
    runtimes: &[(Version, PathBuf)],
    allow_prerelease: bool,
) -> Option<(Version, PathBuf)> {
    runtimes
        .iter()
        .filter(|(v, _)| (!v.is_prerelease() || allow_prerelease) && constraint_allows(m, *v))
        .max_by_key(|(v, _)| *v)
        .map(|(v, p)| (*v, p.clone()))
}

/// Inspect an inka executable: parse its trailer/manifest and report whether a
/// compatible runtime is installed. A non-artifact is a hard error (exit 2); a
/// malformed version constraint exits 3; no compatible runtime exits 3.
fn doctor_artifact(path: &Path, json: bool, effective: channel::Channel) {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            ui::log_error(format!("cannot read {}: {e}", path.display()));
            std::process::exit(1);
        }
    };
    let ArtifactInfo {
        manifest,
        entries,
        size,
    } = match parse_artifact(&bytes) {
        Ok(v) => v,
        Err(ArtifactError::NotArtifact(e)) => {
            ui::log_error(format!("{}: {e}", path.display()));
            std::process::exit(2);
        }
        Err(ArtifactError::Malformed(e)) => {
            ui::log_error(format!("{}: {e}", path.display()));
            std::process::exit(3);
        }
    };

    let dirs = runtime_search_dirs();
    let runtimes = installed_parts_all(&dirs);
    // Manifest-governed: the artifact opts into prereleases via `channel=beta`
    // or a prerelease version slot; the effective toolchain channel is a
    // superset.
    let allow_prerelease = manifest.wants_prerelease() || effective == channel::Channel::Beta;
    let selected = select_runtime(&manifest, &runtimes, allow_prerelease);
    // Would a beta tuple satisfy this artifact if it had opted in?
    let beta_alternative = if allow_prerelease {
        None
    } else {
        select_runtime(&manifest, &runtimes, true)
    };
    let unpacked: usize = entries.iter().map(|(_, n)| *n).sum();

    if json {
        let doc = serde_json::json!({
            "path": path.display().to_string(),
            "size": size,
            "format": "INKFOOT5",
            "module": manifest.module,
            "runtime": required_runtime(&manifest),
            "tested_against": manifest.tested.map(|v| v.to_string()),
            "requires": manifest.requires,
            "path_base": manifest.path_base,
            "channel": manifest.channel,
            "permissions": manifest.perms,
            "files": entries
                .iter()
                .map(|(p, n)| serde_json::json!({ "path": p, "size": n }))
                .collect::<Vec<_>>(),
            "selected_runtime": selected
                .as_ref()
                .map(|(v, p)| serde_json::json!({ "version": v.to_string(), "path": p.display().to_string() })),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
        );
        if selected.is_none() {
            std::process::exit(3);
        }
        return;
    }

    ui::title("doctor");
    ui::section("Artifact");
    ui::row("path", path.display());
    ui::row("size", ui::human_size(size as u64));
    ui::row("format", "INKFOOT5");
    ui::row("module", &manifest.module);
    ui::row("runtime", required_runtime(&manifest));
    if let Some(t) = manifest.tested {
        ui::row("tested-against", t.to_string());
    }
    if !manifest.requires.is_empty() {
        ui::row("requires", &manifest.requires);
    }
    if let Some(pb) = &manifest.path_base {
        ui::row("path-base", pb);
    }
    if let Some(ch) = &manifest.channel {
        ui::row("channel", ch);
    }

    ui::section("Permissions");
    if manifest.perms.trim().is_empty() {
        ui::warn_row("grants", "none (deny-by-default)");
    } else {
        for line in manifest.perms.lines() {
            ui::row("", line);
        }
    }

    ui::section("Payload");
    ui::row("files", entries.len().to_string());
    ui::row("unpacked", ui::human_size(unpacked as u64));
    let shown = 5.min(entries.len());
    for (p, n) in entries.iter().take(shown) {
        println!(
            "  {} {} {}",
            colors::gray("·"),
            p,
            colors::gray(format!("({})", ui::human_size(*n as u64)))
        );
    }
    if entries.len() > shown {
        println!(
            "  {}",
            colors::gray(format!("… {} more", entries.len() - shown))
        );
    }

    ui::section("Runtime");
    match &selected {
        Some((v, p)) => ui::ok("compatible", format!("{v}  {}", colors::gray(p.display()))),
        None => ui::bad_row("compatible", "none found"),
    }
    for (v, p) in &runtimes {
        let marker = if Some(*v) == selected.as_ref().map(|(v, _)| *v) {
            colors::green("→").to_string()
        } else {
            " ".to_string()
        };
        let pre = if v.is_prerelease() {
            colors::gray(" (beta)").to_string()
        } else {
            String::new()
        };
        println!(
            "  {marker} {:<10} {}{}",
            colors::green(v.to_string()),
            colors::gray(p.display()),
            pre
        );
    }
    if selected.is_none() {
        if beta_alternative.is_some() {
            ui::hint(
                "a beta runtime satisfies this artifact; rebuild with `inka build --beta` \
                 (or run with `inka run --beta`) to opt in",
            );
        }
        ui::hint("run `inka update` to install a compatible runtime");
        ui::status_bad("no compatible runtime");
        std::process::exit(3);
    }
    ui::status_ok("artifact ok");
}

/// Human-readable runtime requirement from a manifest's constraint slots.
fn required_runtime(m: &inka_format::Manifest) -> String {
    if let Some(e) = m.exact {
        format!("inka_runtime == {e}")
    } else if let Some(g) = m.gt {
        format!("inka_runtime > {g}")
    } else if let Some(x) = m.min {
        format!("inka_runtime >= {x}")
    } else {
        "inka_runtime (any)".to_string()
    }
}

/// The toolchain identity shown by `doctor`: `release (short-hash)` when the
/// release pipeline baked a commit, else just the release (dev build).
fn toolchain_label() -> String {
    match channel::build_commit() {
        Some(h) => format!("{} ({h})", channel::release_version()),
        None => channel::release_version().to_string(),
    }
}

fn doctor_machine(effective: channel::Channel) {
    let dirs = runtime_search_dirs();
    let runtimes = installed_parts_all(&dirs);
    let mut warnings: Vec<String> = Vec::new();
    let mut problems: Vec<(String, String)> = Vec::new();

    ui::title("doctor");
    ui::section("Toolchain");
    ui::row("toolchain", toolchain_label());
    ui::row("channel", effective.as_str());
    // Cross-check the installer-written VERSION marker when present.
    if let Some(dir) = crate::update::toolchain_dir() {
        let marker = crate::update::installed_toolchain_version(&dir);
        if !marker.is_empty() && marker != channel::release_version() {
            ui::warn_row(
                "version",
                format!(
                    "marker {marker} does not match the binary {}",
                    channel::release_version()
                ),
            );
            warnings.push(format!(
                "toolchain VERSION marker ({marker}) does not match the binary ({})",
                channel::release_version()
            ));
        }
    }

    ui::section("Runtimes");
    if runtimes.is_empty() {
        ui::bad_row("installed", "none");
        problems.push((
            "no runtimes installed".to_string(),
            "run `inka update`".to_string(),
        ));
    } else {
        // Stable selection ignores prerelease tuples unless the effective
        // channel is beta, mirroring the launcher's artifact selection.
        let selected = if effective == channel::Channel::Beta {
            runtimes.last().map(|(v, _)| *v)
        } else {
            runtimes
                .iter()
                .rev()
                .find(|(v, _)| !v.is_prerelease())
                .map(|(v, _)| *v)
        };
        for (v, p) in &runtimes {
            let marker = if Some(*v) == selected {
                colors::green("→").to_string()
            } else {
                " ".to_string()
            };
            let pre = if v.is_prerelease() {
                colors::gray(" (beta)").to_string()
            } else {
                String::new()
            };
            println!(
                "  {marker} {:<10} {}{}",
                colors::green(v.to_string()),
                colors::gray(p.display()),
                pre
            );
        }
    }

    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    ui::section("Project (cwd)");
    println!("  {}", colors::gray(cwd.display()));

    let configs: Vec<&str> = ["package.json", "deno.json", "deno.jsonc"]
        .into_iter()
        .filter(|f| cwd.join(f).is_file())
        .collect();
    if configs.is_empty() {
        ui::warn_row("config", "(no package.json/deno.json/deno.jsonc)");
        warnings.push("no project config (package.json/deno.json)".to_string());
    } else {
        ui::ok("config", configs.join(", "));
    }

    if cwd.join("node_modules").is_dir() {
        ui::ok("node_modules", "present");
    } else {
        ui::warn_row("node_modules", "absent");
        warnings.push("node_modules is absent; dependency imports may fail".to_string());
    }

    let deno_dir = default_deno_dir();
    if deno_dir.is_dir() {
        ui::ok(
            "DENO_DIR",
            format!(
                "{}  (remote: {}, npm: {})",
                deno_dir.display(),
                presence(deno_dir.join("remote").is_dir()),
                presence(deno_dir.join("npm").is_dir()),
            ),
        );
    } else {
        ui::warn_row("DENO_DIR", format!("{} (absent)", deno_dir.display()));
        warnings.push(format!("DENO_DIR {} is absent", deno_dir.display()));
    }

    if cfg!(feature = "bundle") {
        ui::ok("bundling", "available");
    } else {
        ui::bad_row("bundling", "unavailable (built without `bundle`)");
        problems.push((
            "inka was built without the `bundle` feature; `inka build` cannot bundle".to_string(),
            "use an official release or rebuild with `--features bundle`".to_string(),
        ));
    }

    match launcher_path() {
        Some(p) => ui::ok("launcher", p.display().to_string()),
        None => {
            ui::bad_row("launcher", "not found");
            problems.push((
                "the `inka-launcher` binary was not found".to_string(),
                "reinstall the toolchain, or set INKA_LAUNCHER".to_string(),
            ));
        }
    }

    if problems.is_empty() && warnings.is_empty() {
        ui::status_ok("ready");
    } else {
        if !problems.is_empty() {
            ui::status_bad(format!("{} problem(s)", problems.len()));
        }
        for (msg, hint) in &problems {
            println!("  {} {}", colors::red("✗"), msg);
            println!("    {} {}", colors::cyan("hint:"), colors::gray(hint));
        }
        if !warnings.is_empty() {
            ui::status_warn(format!("{} warning(s)", warnings.len()));
        }
        for w in &warnings {
            println!("  {} {}", colors::yellow("!"), w);
        }
    }
}

fn presence(present: bool) -> &'static str {
    if present {
        "present"
    } else {
        "absent"
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

    #[test]
    fn deno_dir_prefers_env_then_home() {
        assert_eq!(
            deno_dir_root(Some(os("/custom")), Some(os("/home/u"))),
            PathBuf::from("/custom")
        );
        assert_eq!(
            deno_dir_root(Some(os("")), Some(os("/home/u"))),
            PathBuf::from("/home/u/.cache/deno")
        );
        assert_eq!(
            deno_dir_root(None, Some(os("/home/u"))),
            PathBuf::from("/home/u/.cache/deno")
        );
        assert_eq!(deno_dir_root(None, None), PathBuf::from("./.cache/deno"));
    }

    /// A minimal INKFOOT5 image: launcher stub + archive + manifest + footer.
    fn artifact_image(manifest: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
        let owned: Vec<(String, Vec<u8>)> = files
            .iter()
            .map(|(p, b)| (p.to_string(), b.to_vec()))
            .collect();
        let archive = inka_format::encode_archive(&owned);
        let mut image = vec![0x7f, b'E', b'L', b'F'];
        image.extend_from_slice(&archive);
        image.extend_from_slice(manifest.as_bytes());
        image.extend_from_slice(&inka_format::encode_footer(
            archive.len() as u64,
            manifest.len() as u64,
        ));
        image
    }

    #[test]
    fn parse_artifact_reads_manifest_and_index() {
        let image = artifact_image(
            "runtime=inka_runtime>=0.266.2\npermissions=all\nallow-read=./data\nmodule=main.js\n",
            &[
                ("main.js", b"console.log(1)"),
                ("node_modules/x/index.js", b"x"),
            ],
        );
        let info = parse_artifact(&image).unwrap();
        assert_eq!(info.manifest.module, "main.js");
        assert_eq!(info.manifest.perms, "permissions=all\nallow-read=./data");
        assert_eq!(info.manifest.min, Some(Version::new(0, 266, 2)));
        assert_eq!(info.entries.len(), 2);
        assert_eq!(info.entries[1], ("node_modules/x/index.js".to_string(), 1));
        assert_eq!(info.size, image.len());
    }

    #[test]
    fn parse_artifact_rejects_non_artifact() {
        assert!(matches!(
            parse_artifact(b"hello world, not an artifact\n"),
            Err(ArtifactError::NotArtifact(_))
        ));
    }

    #[test]
    fn parse_artifact_rejects_malformed_version() {
        let image = artifact_image("runtime=inka_runtime>=nope\n", &[("main.js", b"x")]);
        assert!(matches!(
            parse_artifact(&image),
            Err(ArtifactError::Malformed(_))
        ));
    }

    #[test]
    fn select_runtime_respects_floor_and_cap() {
        let m = parse_manifest(b"runtime=inka_runtime>=0.266.2\ntested-against=0.266.4\n");
        let runtimes: Vec<(Version, PathBuf)> = [0, 1, 2, 4, 5]
            .into_iter()
            .map(|c| (Version::new(0, 266, c), PathBuf::from(format!("/r/{c}"))))
            .collect();
        // Newest satisfying the cap is 0.266.4 (0.266.5 is above tested-against).
        assert_eq!(
            select_runtime(&m, &runtimes, false).unwrap().0,
            Version::new(0, 266, 4)
        );

        // No satisfying runtime -> None.
        let m_exact = parse_manifest(b"runtime=inka_runtime==0.266.9\n");
        assert!(select_runtime(&m_exact, &runtimes, false).is_none());

        // A prerelease tuple is skipped unless the beta channel is allowed.
        let mut with_beta = runtimes.clone();
        with_beta.push((
            parse_version("0.267.2-beta.1").unwrap(),
            PathBuf::from("/r/beta"),
        ));
        let open = parse_manifest(b"module=main.js\n");
        assert_eq!(
            select_runtime(&open, &with_beta, false).unwrap().0,
            Version::new(0, 266, 5)
        );
        assert_eq!(
            select_runtime(&open, &with_beta, true).unwrap().0,
            parse_version("0.267.2-beta.1").unwrap()
        );
    }

    #[test]
    fn required_runtime_renders_each_operator() {
        assert_eq!(
            required_runtime(&parse_manifest(b"runtime=inka_runtime>=0.266.2\n")),
            "inka_runtime >= 0.266.2"
        );
        assert_eq!(
            required_runtime(&parse_manifest(b"runtime=inka_runtime>0.266.2\n")),
            "inka_runtime > 0.266.2"
        );
        assert_eq!(
            required_runtime(&parse_manifest(b"runtime=inka_runtime==0.266.2\n")),
            "inka_runtime == 0.266.2"
        );
        assert_eq!(
            required_runtime(&parse_manifest(b"module=main.js\n")),
            "inka_runtime (any)"
        );
    }
}

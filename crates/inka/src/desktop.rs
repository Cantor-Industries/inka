// inka desktop: package a web app into a desktop application that shares the
// machine's inka runtime.
//
// Layout produced (Linux; Windows uses `.exe`/`.dll` via `platform`):
//
//   <App>/
//     <App>            laufey backend (window + system webview), renamed
//     <App>.so         per-app shim, carrying the payload as the `inka` binary
//                      section (loads the shared libinka_runtime)
//     runtime-version  runtime tuple the shim should load (optional)
//     <id>.desktop     desktop entry
//   <App>.tar.gz
//
// The heavy Deno/V8 engine is NOT copied: it lives once in the inka runtime
// directory and every desktop app reuses it.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::help::{self, Mode};
use crate::platform;
use crate::ui;

/// laufey backend release inka is pinned to (matches `laufey = 0.7.0`).
pub(crate) const LAUFEY_VERSION: &str = "0.7.0";
/// Marker written at the root of an app dir we generated, so a later package
/// build may safely clear it (and nothing else).
const APP_DIR_MARKER: &str = ".inka-desktop-app";

/// Pinned SHA-256 digests for laufey backend archives (trust anchor). Kept in
/// sync with `denoland/deno`'s `cli/laufey_sums.lock` for the pinned release, so
/// a download is verified against a value checked into this repo rather than
/// the release's own (unsigned) `SHA256SUMS`.
pub(crate) const LAUFEY_SUMS: &[(&str, &str)] = &[
    (
        "laufey-cef-aarch64-apple-darwin.tar.gz",
        "edc9d8d68016417f726f0a7268240c015017439036e05cf16c464856cd423f47",
    ),
    (
        "laufey-cef-aarch64-unknown-linux-gnu.tar.gz",
        "11cc33bca5a58bb47dc300a0097447c8ca41f61d18cbabb906023cfc4dc35fb9",
    ),
    (
        "laufey-cef-x86_64-apple-darwin.tar.gz",
        "d2e6bcf24cf256e477ba59a8e07417ae7e4830d803c875dd3df4d89a163b09cb",
    ),
    (
        "laufey-cef-x86_64-pc-windows-msvc.zip",
        "4d23f2d215370242e5c36ee7b89f6dcf7e3d79ea44bc3c50e3e6acaf837d73ad",
    ),
    (
        "laufey-cef-x86_64-unknown-linux-gnu.tar.gz",
        "38359a26bec39f3114c81ef83cace7d351c002f8640bb01426bb7630042c02f3",
    ),
    (
        "laufey-webview-aarch64-apple-darwin.tar.gz",
        "f1ac94af61bbc3c64bb4e7742b8fd3333704ce4546c2b279616a0f00ae5e6422",
    ),
    (
        "laufey-webview-aarch64-pc-windows-msvc.zip",
        "6dfe651589326a8e5e00d08af43abecad3040a32a3c9314010cc5f9edcb50812",
    ),
    (
        "laufey-webview-aarch64-unknown-linux-gnu.tar.gz",
        "bd7d7828a87793b26ec3234f36b13cdb0afb762daa94d7461c37de366da9accc",
    ),
    (
        "laufey-webview-x86_64-apple-darwin.tar.gz",
        "46dd0b4314d0ed5f93f5ed16b3b4c9e1faf051fc6fccb373f9e233c253d49aa2",
    ),
    (
        "laufey-webview-x86_64-pc-windows-msvc.zip",
        "f27ae3b90f0f527ef2c32575e5af7862d95e1400a98561a06a37ce8ffffaa53a",
    ),
    (
        "laufey-webview-x86_64-unknown-linux-gnu.tar.gz",
        "6f4c5e05f934128c1d77f7a1cc8e859331171cf8a89a0d2e19f5c07cf081b235",
    ),
    (
        "laufey-winit-aarch64-apple-darwin.tar.gz",
        "57481c5f5759f782c11c43f1c14133424c869123f1b42431f3138f7cb2e77651",
    ),
    (
        "laufey-winit-aarch64-pc-windows-msvc.zip",
        "955db95103ee4656899dff7e507e01a194d5f670ec1129267da0b94216c0f50e",
    ),
    (
        "laufey-winit-aarch64-unknown-linux-gnu.tar.gz",
        "cee7fac6d66faf043df09cceea80233cc41ce39fe317a72c80b778cce5568c93",
    ),
    (
        "laufey-winit-x86_64-apple-darwin.tar.gz",
        "f05cdeddda3b30caa6dec4b258031c161ad3c5356c75794c5d0cc352b95ba5dd",
    ),
    (
        "laufey-winit-x86_64-pc-windows-msvc.zip",
        "ee23fe21e1ff75a693aeb2aa48b07f0d95c449a1036e6acfaf1759436a878eab",
    ),
    (
        "laufey-winit-x86_64-unknown-linux-gnu.tar.gz",
        "173c091ee71bcc3b6e9e9811c6e818f8f3ac92f72ab8e3ac1daaad0076aa56be",
    ),
];

pub(crate) fn laufey_archive_name(backend: &str) -> String {
    let archive_backend = if backend == "raw" { "winit" } else { backend };
    let target = platform::laufey_target();
    let ext = if target.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    format!("laufey-{archive_backend}-{target}.{ext}")
}

fn laufey_release_base() -> String {
    format!("https://github.com/littledivy/laufey/releases/download/v{LAUFEY_VERSION}")
}

/// Inka's own laufey cache root (`$XDG_CACHE_HOME/inka/laufey`, else
/// `~/.cache/inka/laufey`; `%LOCALAPPDATA%\inka\laufey` on Windows).
fn inka_laufey_cache() -> Option<PathBuf> {
    platform::cache_root().map(|c| c.join("inka/laufey"))
}

fn usage() -> ! {
    eprintln!("usage: inka desktop [entry] [options]");
    ui::hint("run `inka desktop --help` for details");
    std::process::exit(2);
}

fn fail(msg: &str) -> ! {
    ui::log_error(msg);
    std::process::exit(1);
}

/// An app icon: a single image, or Deno-style `{ path, size }` set. A set lets
/// Windows build a multi-resolution `.ico`.
enum IconArg {
    Single(PathBuf),
    Set(Vec<(PathBuf, u32)>),
}

struct Args {
    entry: Option<PathBuf>,
    output: Option<PathBuf>,
    app_name: Option<String>,
    identifier: Option<String>,
    backend: Option<String>,
    icon: Option<IconArg>,
    payload: Option<PathBuf>,
    no_bundle: bool,
    minify: bool,
    sourcemap: bool,
    hmr: bool,
    inspect: Option<Option<String>>,
    inspect_brk: Option<Option<String>>,
    inspect_wait: Option<Option<String>>,
    external: Vec<String>,
    app_version: Option<String>,
    release_base: Option<String>,
    error_reporting: Option<String>,
    installer: bool,
    engine_base: Option<String>,
    dev_command: Option<String>,
    deep_links: Vec<String>,
    compress: Option<String>,
}

fn parse_args(args: &[String]) -> Args {
    let mut a = Args {
        entry: None,
        output: None,
        app_name: None,
        identifier: None,
        backend: None,
        icon: None,
        payload: None,
        no_bundle: false,
        minify: false,
        sourcemap: false,
        hmr: false,
        inspect: None,
        inspect_brk: None,
        inspect_wait: None,
        external: Vec::new(),
        app_version: None,
        release_base: None,
        error_reporting: None,
        installer: false,
        engine_base: None,
        dev_command: None,
        deep_links: Vec::new(),
        compress: None,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let next = |it: &mut std::slice::Iter<'_, String>, flag: &str| -> String {
            it.next().cloned().unwrap_or_else(|| {
                ui::log_error(format!("{flag} requires a value"));
                usage()
            })
        };
        match arg.as_str() {
            "-h" | "--help" => {
                help::print(help::desktop(), Mode::Long);
                std::process::exit(0);
            }
            "-o" | "--output" => a.output = Some(PathBuf::from(next(&mut it, arg))),
            "--name" => a.app_name = Some(next(&mut it, arg)),
            "--identifier" => a.identifier = Some(next(&mut it, arg)),
            "--backend" => a.backend = Some(next(&mut it, arg)),
            "--icon" => a.icon = Some(IconArg::Single(PathBuf::from(next(&mut it, arg)))),
            "--payload" => a.payload = Some(PathBuf::from(next(&mut it, arg))),
            "--external" => a.external.push(next(&mut it, arg)),
            "--no-bundle" => a.no_bundle = true,
            "--minify" => a.minify = true,
            "--sourcemap" => a.sourcemap = true,
            "--hmr" => a.hmr = true,
            "--inspect" => a.inspect = Some(None),
            "--inspect-brk" => a.inspect_brk = Some(None),
            "--inspect-wait" => a.inspect_wait = Some(None),
            other if other.starts_with("--inspect=") => {
                a.inspect = Some(Some(other["--inspect=".len()..].to_string()));
            }
            other if other.starts_with("--inspect-brk=") => {
                a.inspect_brk = Some(Some(other["--inspect-brk=".len()..].to_string()));
            }
            other if other.starts_with("--inspect-wait=") => {
                a.inspect_wait = Some(Some(other["--inspect-wait=".len()..].to_string()));
            }
            "--app-version" => a.app_version = Some(next(&mut it, arg)),
            "--release-base" => a.release_base = Some(next(&mut it, arg)),
            "--error-reporting" => a.error_reporting = Some(next(&mut it, arg)),
            "--installer" => a.installer = true,
            "--engine-base" => a.engine_base = Some(next(&mut it, arg)),
            "--dev-command" => a.dev_command = Some(next(&mut it, arg)),
            "--deep-link" => a.deep_links.push(next(&mut it, arg)),
            "--compress" => a.compress = Some("gzip".to_string()),
            other if other.starts_with("--deep-link=") => {
                a.deep_links.push(other["--deep-link=".len()..].to_string());
            }
            other if other.starts_with("--compress=") => {
                a.compress = Some(other["--compress=".len()..].to_string());
            }
            other if other.starts_with("--external=") => {
                a.external.push(other["--external=".len()..].to_string());
            }
            other if other.starts_with('-') => {
                ui::log_error(format!("unknown option '{other}'"));
                usage();
            }
            other => {
                if a.entry.is_some() {
                    ui::log_error("inka desktop takes at most one entry file");
                    usage();
                }
                a.entry = Some(PathBuf::from(other));
            }
        }
    }
    a
}

/// The laufey backend executable name for a backend kind (`.exe` on Windows).
fn backend_exe(backend: &str) -> String {
    let base = match backend {
        "cef" => "laufey",
        "webview" => "laufey_webview",
        // `raw` ships upstream as `winit`.
        _ => "laufey_winit",
    };
    format!("{base}{}", platform::exe_suffix())
}

/// Where Deno/inka cache laufey backends: `$DENO_DIR` (else `~/.cache/deno`).
fn laufey_cache_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(d) = env::var_os("INKA_LAUFEY_CACHE") {
        roots.push(PathBuf::from(d));
    }
    if let Some(d) = env::var_os("DENO_DIR") {
        if !d.is_empty() {
            roots.push(PathBuf::from(d).join("laufey"));
        }
    }
    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        roots.push(home.join(".cache/deno/laufey"));
        roots.push(home.join(".cache/inka/laufey"));
    }
    roots
}

/// Recursively search `dir` for a file named `name` (bounded depth).
fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    fn walk(dir: &Path, name: &str, depth: usize) -> Option<PathBuf> {
        if depth > 6 {
            return None;
        }
        let rd = fs::read_dir(dir).ok()?;
        let mut subdirs = Vec::new();
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_file() && p.file_name().is_some_and(|n| n == name) {
                return Some(p);
            }
            if p.is_dir() {
                subdirs.push(p);
            }
        }
        subdirs.into_iter().find_map(|d| walk(&d, name, depth + 1))
    }
    walk(dir, name, 0)
}

/// Download (once), checksum-verify, and unpack the pinned laufey backend into
/// inka's cache, returning the backend executable path.
///
/// Extraction is hardened (`crate::archive`) and staged in a sibling directory
/// that is atomically renamed into place, so a crash or a concurrent build
/// never sees a half-populated cache. The `.downloaded` marker is written
/// inside the staging dir so it is published with the payload.
fn download_laufey(backend: &str) -> Result<PathBuf, String> {
    let cache = inka_laufey_cache()
        .ok_or_else(|| "cannot determine a cache directory (set HOME)".to_string())?;
    let parent = cache.join(LAUFEY_VERSION).join(backend);
    let target = platform::laufey_target();
    let dir = parent.join(target);
    let exe = backend_exe(backend);
    if dir.join(".downloaded").is_file() {
        if let Some(p) = find_file(&dir, &exe) {
            return Ok(p);
        }
    }
    let archive = laufey_archive_name(backend);
    let expected = LAUFEY_SUMS
        .iter()
        .find(|(n, _)| *n == archive)
        .map(|(_, h)| *h)
        .ok_or_else(|| format!("no pinned checksum for {archive}"))?;
    fs::create_dir_all(&parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    let tmp = parent.join(format!(".{archive}.tmp{}", std::process::id()));
    ui::info(format!(
        "downloading laufey '{backend}' backend ({archive})"
    ));
    if let Err(e) = crate::download_file(&laufey_release_base(), &archive, &tmp) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("failed to download {archive}: {e}"));
    }
    let actual = crate::sha256_file(&tmp)?;
    if actual != expected {
        let _ = fs::remove_file(&tmp);
        return Err(format!(
            "checksum mismatch for {archive}: expected {expected}, got {actual}"
        ));
    }

    let staging = parent.join(format!(
        "{target}.staging-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&staging);
    let publish = (|| {
        crate::archive::extract(&archive, &tmp, &staging)?;
        if find_file(&staging, &exe).is_none() {
            return Err(format!("'{exe}' not found in the {archive} archive"));
        }
        fs::write(staging.join(".downloaded"), format!("v{LAUFEY_VERSION}\n"))
            .map_err(|e| format!("cannot write {}: {e}", staging.display()))?;
        if dir.exists() {
            let _ = fs::remove_dir_all(&dir);
        }
        fs::rename(&staging, &dir).map_err(|e| format!("cannot publish {}: {e}", dir.display()))
    })();
    let _ = fs::remove_file(&tmp);
    if let Err(e) = publish {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }
    find_file(&dir, &exe).ok_or_else(|| format!("'{exe}' not found in the {archive} archive"))
}

fn resolve_backend(backend: &str) -> PathBuf {
    if let Ok(p) = env::var("INKA_LAUFEY_BACKEND") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return p;
        }
    }
    let exe = backend_exe(backend);
    if let Some(dir) = env::var_os("LAUFEY_DEV_DIR") {
        let dir = PathBuf::from(dir);
        for cand in [
            dir.join(format!("result/{exe}")),
            dir.join(format!("target/release/{exe}")),
            dir.join(format!("target/debug/{exe}")),
            dir.join(&exe),
        ] {
            if cand.is_file() {
                return cand;
            }
        }
    }
    for root in laufey_cache_roots() {
        let cand = root
            .join(LAUFEY_VERSION)
            .join(backend)
            .join(platform::laufey_target())
            .join(&exe);
        if cand.is_file() {
            return cand;
        }
    }
    match download_laufey(backend) {
        Ok(p) => p,
        Err(e) => {
            ui::log_error(format!(
                "no laufey '{backend}' backend available (pinned {LAUFEY_VERSION}): {e}"
            ));
            ui::hint("set INKA_LAUFEY_BACKEND to a backend binary, or LAUFEY_DEV_DIR to a laufey checkout");
            std::process::exit(1);
        }
    }
}

/// The per-app shim cdylib, next to the running `inka` binary (or overridden).
fn resolve_shim() -> PathBuf {
    if let Ok(p) = env::var("INKA_DESKTOP_SHIM") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return p;
        }
        fail(&format!("INKA_DESKTOP_SHIM is not a file: {}", p.display()));
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in [platform::shim_lib_name(), "inka-desktop-shim"] {
                let cand = dir.join(name);
                if cand.is_file() {
                    return cand;
                }
            }
        }
    }
    fail("cannot find the desktop shim; set INKA_DESKTOP_SHIM or keep it next to the inka binary");
}

/// The runtime tuple a packaged app should load: `INKA_DESKTOP_RUNTIME_TUPLE`
/// when set, else the newest installed runtime that advertises the `desktop`
/// capability. A headless dev build of the same tuple is skipped so packaging
/// doesn't pin an engine that can't host the app.
fn installed_runtime_tuple() -> Option<String> {
    if let Ok(t) = env::var("INKA_DESKTOP_RUNTIME_TUPLE") {
        let t = t.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    let runtimes = crate::installed_parts_all(&crate::runtime_search_dirs());
    let mut newest: Option<String> = None;
    for (v, path) in runtimes.iter().rev() {
        if newest.is_none() {
            newest = Some(v.to_string());
        }
        let desktop_ok = crate::runtime_features(path)
            .map(|f| f.split(',').any(|x| x.trim() == "desktop"))
            .unwrap_or(false);
        if desktop_ok {
            return Some(v.to_string());
        }
    }
    if let Some(v) = newest {
        ui::warn(format!(
            "newest installed runtime {v} is not desktop-enabled; run `inka update` \
             (or set INKA_DESKTOP_RUNTIME at launch)"
        ));
        return Some(v);
    }
    None
}

fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() {
        "app".to_string()
    } else {
        cleaned
    }
}

/// Fill unset CLI flags from the resolved `desktop` config (CLI always wins).
/// `output`/`icon` paths are config-relative (the discovered project dir), so
/// they are joined with `cfg.base_dir`.
fn apply_config_defaults(a: &mut Args, cfg: crate::config::DesktopConfig) {
    let base = cfg.base_dir;
    if a.app_name.is_none() {
        a.app_name = cfg.app_name;
    }
    if a.identifier.is_none() {
        a.identifier = cfg.identifier;
    }
    if a.backend.is_none() {
        a.backend = cfg.backend;
    }
    if a.output.is_none() {
        let cfg_output = if cfg!(windows) {
            cfg.output_windows
        } else {
            cfg.output_linux
        };
        a.output = cfg_output.map(|o| base.join(o));
    }
    if a.icon.is_none() {
        use crate::config::DesktopIcon;
        let cfg_icon = if cfg!(windows) {
            cfg.icon_windows
        } else {
            cfg.icon_linux
        };
        a.icon = cfg_icon.map(|i| match i {
            DesktopIcon::Single(p) => IconArg::Single(base.join(p)),
            DesktopIcon::Set(entries) => IconArg::Set(
                entries
                    .into_iter()
                    .map(|(p, s)| (base.join(p), s))
                    .collect(),
            ),
        });
    }
    if a.app_version.is_none() {
        a.app_version = cfg.version;
    }
    if a.release_base.is_none() {
        a.release_base = cfg.release_base;
    }
    if a.error_reporting.is_none() {
        a.error_reporting = cfg.error_reporting;
    }
    if a.deep_links.is_empty() && !cfg.deep_links.is_empty() {
        a.deep_links = cfg.deep_links;
    }
    if a.compress.is_none() {
        a.compress = cfg.compress;
    }
}

/// The laufey backend kinds `inka desktop` knows how to fetch.
fn known_backend(backend: &str) -> bool {
    matches!(backend, "webview" | "cef" | "raw")
}

/// Validate a reverse-DNS bundle identifier (Linux `.desktop` filename and
/// `StartupWMClass`). ASCII alphanumerics plus `.`, `-`, `_`; must be dotted.
fn validate_identifier(id: &str) {
    let valid_chars = id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if id.is_empty()
        || !id.contains('.')
        || !valid_chars
        || id.starts_with('.')
        || id.ends_with('.')
    {
        fail(&format!(
            "invalid identifier '{id}': expected reverse-DNS form like com.acme.app"
        ));
    }
}

/// The shared runtime to run a dev (`--hmr`) app with: `$INKA_DESKTOP_RUNTIME`,
/// else the newest installed runtime that advertises the `desktop` capability.
fn resolve_desktop_runtime() -> PathBuf {
    if let Ok(p) = env::var("INKA_DESKTOP_RUNTIME") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return p;
        }
    }
    let runtimes = crate::installed_parts_all(&crate::runtime_search_dirs());
    for (_, path) in runtimes.iter().rev() {
        let desktop_ok = crate::runtime_features(path)
            .map(|f| f.split(',').any(|x| x.trim() == "desktop"))
            .unwrap_or(false);
        if desktop_ok {
            return path.clone();
        }
    }
    fail("no desktop-enabled runtime installed; run `inka update` or set INKA_DESKTOP_RUNTIME");
}

/// Parse an `--inspect` address (`host:port` or bare `port`), defaulting to the
/// Node-style `127.0.0.1:9229`.
fn parse_inspect_addr(value: Option<&str>) -> String {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        None => "127.0.0.1:9229".to_string(),
        Some(v) if v.chars().all(|c| c.is_ascii_digit()) => format!("127.0.0.1:{v}"),
        Some(v) => v.to_string(),
    }
}

/// A child framework dev server spawned for external `--hmr` mode. Killed on
/// drop so it can't outlive the desktop app.
struct DevServer {
    child: std::process::Child,
    url: String,
}

impl Drop for DevServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Prefix of the generated dev/inspect entrypoint written into the project.
/// Swept on start and removed on exit (a Ctrl-C still runs the `status` path
/// here, since the CLI blocks on the backend).
const DEV_ENTRY_PREFIX: &str = ".inka-desktop-entry-";

/// Remove temp entrypoints leaked by an interrupted previous run.
fn sweep_stale_dev_entries(dir: &Path) {
    if let Ok(rd) = fs::read_dir(dir) {
        for ent in rd.flatten() {
            if ent
                .file_name()
                .to_string_lossy()
                .starts_with(DEV_ENTRY_PREFIX)
            {
                let _ = fs::remove_file(ent.path());
            }
        }
    }
}

/// Write a generated dev entrypoint into `base`, returning its file name
/// (relative to `base`).
fn write_dev_entry(base: &Path, code: &str) -> Result<String, String> {
    let name = format!("{DEV_ENTRY_PREFIX}{}.ts", std::process::id());
    let path = base.join(&name);
    fs::write(&path, code).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(name)
}

/// Whether `dir/package.json` declares a `scripts.<script>` entry.
fn has_package_script(dir: &Path, script: &str) -> bool {
    let Ok(text) = fs::read_to_string(dir.join("package.json")) else {
        return false;
    };
    let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    pkg.get("scripts").and_then(|s| s.get(script)).is_some()
}

/// The package manager implied by the project's lockfile (default `npm`).
fn detect_package_manager(dir: &Path) -> &'static str {
    if dir.join("bun.lock").exists() || dir.join("bun.lockb").exists() {
        "bun"
    } else if dir.join("pnpm-lock.yaml").exists() {
        "pnpm"
    } else if dir.join("yarn.lock").exists() {
        "yarn"
    } else {
        "npm"
    }
}

/// Whether `dir/deno.json[c]` declares a task named `task`.
fn has_deno_task(dir: &Path, task: &str) -> bool {
    let Ok(text) = fs::read_to_string(dir.join("deno.json"))
        .or_else(|_| fs::read_to_string(dir.join("deno.jsonc")))
    else {
        return false;
    };
    let Ok(cfg) = crate::config::parse_jsonc(&text) else {
        return false;
    };
    cfg.get("tasks").and_then(|t| t.get(task)).is_some()
}

/// Resolve the project's dev-server command for external `--hmr`: an explicit
/// `--dev-command`, else `package.json` `scripts.dev` via the lockfile's
/// package manager, else `deno task dev`.
fn resolve_dev_command(dir: &Path, explicit: Option<&str>) -> Result<Vec<String>, String> {
    if let Some(cmd) = explicit {
        let parts: Vec<String> = cmd.split_whitespace().map(str::to_string).collect();
        if parts.is_empty() {
            return Err("--dev-command is empty".to_string());
        }
        return Ok(parts);
    }
    if has_package_script(dir, "dev") {
        return Ok(vec![
            detect_package_manager(dir).to_string(),
            "run".into(),
            "dev".into(),
        ]);
    }
    if has_deno_task(dir, "dev") {
        return Ok(vec!["deno".into(), "task".into(), "dev".into()]);
    }
    Err(
        "no `dev` script found: add one to package.json (`scripts.dev`) or deno.json \
         (`tasks.dev`), or pass --dev-command <cmd>"
            .to_string(),
    )
}

/// Spawn the project's dev server and return once it prints its local URL
/// (15s budget). Its output is forwarded to stderr; the reader thread lives as
/// long as the child.
fn spawn_dev_server(cmd: &[String], dir: &Path, name: &str) -> Result<DevServer, String> {
    use std::io::BufRead;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| {
            format!(
                "could not start the {name} dev server ({}): {e}",
                cmd.join(" ")
            )
        })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "could not capture the dev server's output".to_string())?;

    let slot: Arc<(Mutex<Option<String>>, Condvar)> = Arc::new((Mutex::new(None), Condvar::new()));
    let done = Arc::new(AtomicBool::new(false));
    let thread_slot = slot.clone();
    let thread_done = done.clone();
    std::thread::Builder::new()
        .name("inka-dev-server".to_string())
        .spawn(move || {
            let reader = std::io::BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                eprintln!("{line}");
                if let Some(url) = crate::framework::parse_dev_server_url(&line) {
                    let (m, cv) = &*thread_slot;
                    let mut guard = m.lock().unwrap();
                    if guard.is_none() {
                        *guard = Some(url);
                        cv.notify_all();
                    }
                }
            }
            let (m, cv) = &*thread_slot;
            let _guard = m.lock().unwrap();
            thread_done.store(true, Ordering::Release);
            cv.notify_all();
        })
        .map_err(|e| format!("could not start the dev server reader: {e}"))?;

    let deadline = Instant::now() + Duration::from_secs(15);
    let (m, cv) = &*slot;
    let mut guard = m.lock().unwrap();
    while guard.is_none() && !done.load(Ordering::Acquire) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let (next, _) = cv.wait_timeout(guard, remaining).unwrap();
        guard = next;
    }
    let url = guard.clone();
    drop(guard);

    match url {
        Some(url) => Ok(DevServer { child, url }),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            if done.load(Ordering::Acquire) {
                Err(format!(
                    "the {name} dev server exited before printing a local URL"
                ))
            } else {
                Err(format!(
                    "the {name} dev server did not print a local URL within 15s"
                ))
            }
        }
    }
}

/// `inka desktop --hmr` / `--inspect*`: run a source tree through the shared
/// desktop runtime and the laufey backend, without packaging. A directory entry
/// detects a framework (in-runtime Vite dev, external dev server, or the
/// production entrypoint for `--inspect`); a file entry runs with inka's own
/// V8 HMR when `--hmr` is set. Never returns.
fn run_desktop_dev(a: &Args, entry: &Path, app_name: &str, base: &Path, backend: &Path) -> ! {
    // A directory entry *is* the project tree; a file entry lives inside `base`.
    let project = if entry.is_dir() { entry } else { base };
    let project_abs = project
        .canonicalize()
        .unwrap_or_else(|e| fail(&format!("cannot resolve {}: {e}", project.display())));
    let base_abs = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());

    // Run the backend with the project as CWD so framework tooling (Vite) finds
    // its config/root in the source tree, not the invocation directory.
    let _ = env::set_current_dir(&project_abs);

    let mut temp_entry: Option<PathBuf> = None;
    let mut dev_server: Option<DevServer> = None;
    let mut hmr_dir: Option<PathBuf> = None;
    let mut dev_url: Option<String> = None;
    let mut perms: Option<String> = None;

    let entry_rel = if entry.is_dir() {
        sweep_stale_dev_entries(&project_abs);
        let detection = match crate::framework::detect_framework(&project_abs) {
            Ok(Some(d)) => d,
            Ok(None) => fail(
                "could not detect a supported framework in this directory (supported: \
                 Fresh, Astro, Remix, React Router, SvelteKit, Nuxt, SolidStart, TanStack \
                 Start, Vite); pass an explicit entry file instead",
            ),
            Err(e) => fail(&e),
        };
        let code = if a.hmr {
            if let Some(code) = detection.hmr_entrypoint_code() {
                // In-runtime dev server: Vite runs inside the runtime and owns
                // HMR; the webview uses the regular serve-port poll.
                perms = Some("permissions=all".to_string());
                code.to_string()
            } else if detection.needs_external_dev_server() {
                let cmd = resolve_dev_command(&project_abs, a.dev_command.as_deref())
                    .unwrap_or_else(|e| fail(&e));
                ui::info(format!(
                    "running {} dev server: {}",
                    detection.name,
                    cmd.join(" ")
                ));
                let server = spawn_dev_server(&cmd, &project_abs, detection.name)
                    .unwrap_or_else(|e| fail(&e));
                dev_url = Some(server.url.clone());
                dev_server = Some(server);
                perms = Some("permissions=all".to_string());
                crate::framework::NOOP_ENTRYPOINT.to_string()
            } else {
                fail(&format!(
                    "{} has no framework dev server for `--hmr`; package it with \
                     `inka desktop .` instead",
                    detection.name
                ));
            }
        } else {
            if detection.name == "Next.js" {
                fail("Next.js is not supported for desktop dev; pass an explicit entry file");
            }
            // `--inspect` only: run the production entrypoint, building first so
            // its output (`dist`, etc.) exists.
            if detection.build {
                if let Err(e) = run_build(&project_abs) {
                    fail(&e);
                }
            }
            detection.entrypoint_code.clone()
        };
        let name = write_dev_entry(&project_abs, &code).unwrap_or_else(|e| fail(&e));
        temp_entry = Some(project_abs.join(&name));
        name
    } else {
        let entry_abs = entry
            .canonicalize()
            .unwrap_or_else(|e| fail(&format!("cannot resolve {}: {e}", entry.display())));
        let rel = entry_abs
            .strip_prefix(&base_abs)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| {
                fail(&format!(
                    "entry {} is outside the project dir {}",
                    entry_abs.display(),
                    base_abs.display()
                ))
            });
        if a.hmr {
            hmr_dir = Some(base_abs.clone());
        }
        rel
    };
    let runtime = resolve_desktop_runtime();

    ui::title("desktop dev");
    ui::row("entry", &entry_rel);
    if let Some(url) = &dev_url {
        ui::row("dev url", url);
    } else if a.hmr {
        ui::row("watch", project_abs.display());
    }
    ui::row("runtime", runtime.display());
    ui::row("backend", backend.display());

    let mut cmd = Command::new(backend);
    cmd.env("LAUFEY_RUNTIME_PATH", &runtime)
        .env("INKA_DESKTOP_PAYLOAD", &project_abs)
        .env("INKA_DESKTOP_ENTRY", &entry_rel)
        .env("INKA_DESKTOP_APP_NAME", app_name)
        .current_dir(&project_abs);
    if let Some(d) = &hmr_dir {
        cmd.env("INKA_DESKTOP_HMR_DIR", d);
    }
    if let Some(u) = &dev_url {
        cmd.env("INKA_DESKTOP_DEV_URL", u);
    }
    if let Some(p) = &perms {
        cmd.env("INKA_DESKTOP_PERMS", p);
    }

    // Keep the tokio runtime and mux handle alive for the child's lifetime.
    let mut _mux: Option<(tokio::runtime::Runtime, crate::desktop_devtools::MuxHandle)> = None;
    let user_inspect = a
        .inspect
        .as_ref()
        .or(a.inspect_brk.as_ref())
        .or(a.inspect_wait.as_ref());
    if let Some(user_addr) = user_inspect {
        let listen: std::net::SocketAddr = parse_inspect_addr(user_addr.as_deref())
            .parse()
            .unwrap_or_else(|e| fail(&format!("invalid --inspect address: {e}")));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|e| fail(&format!("cannot start the DevTools mux runtime: {e}")));
        let deno_port = crate::desktop_devtools::allocate_random_port()
            .unwrap_or_else(|e| fail(&format!("cannot allocate an inspector port: {e}")));
        // No CEF renderer on webview: point the (unused) CEF leg at a dead
        // port so the mux's CEF endpoints fail cleanly rather than hang.
        let cef_port = crate::desktop_devtools::allocate_random_port()
            .unwrap_or_else(|e| fail(&format!("cannot allocate a renderer port: {e}")));
        let handle = rt
            .block_on(crate::desktop_devtools::spawn_mux(
                crate::desktop_devtools::MuxConfig {
                    listen,
                    deno_internal: ([127, 0, 0, 1], deno_port).into(),
                    cef_internal: ([127, 0, 0, 1], cef_port).into(),
                    inspect_brk: a.inspect_brk.is_some(),
                    wait_for_debugger: a.inspect_brk.is_some() || a.inspect_wait.is_some(),
                },
            ))
            .unwrap_or_else(|e| fail(&format!("cannot start the DevTools mux: {e}")));
        let mux = handle.listen.to_string();
        ui::info(format!(
            "DevTools mux on ws://{mux}  (open chrome://inspect; use /deno for the runtime isolate)"
        ));
        cmd.env("INKA_DESKTOP_MUX_WS", &mux).env(
            "INKA_DESKTOP_INSPECT_INTERNAL_PORT",
            format!("127.0.0.1:{deno_port}"),
        );
        if a.inspect_brk.is_some() {
            cmd.env("INKA_DESKTOP_INSPECT_BRK", "1");
        }
        if a.inspect_wait.is_some() {
            cmd.env("INKA_DESKTOP_INSPECT_WAIT", "1");
        }
        _mux = Some((rt, handle));
    }

    let status = cmd.status();
    drop(dev_server);
    if let Some(p) = &temp_entry {
        let _ = fs::remove_file(p);
    }
    drop(_mux);
    match status {
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => fail(&format!("could not launch the laufey backend: {e}")),
    }
}

pub fn cmd_desktop(args: &[String]) {
    let mut a = parse_args(args);
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // `deno.json` `desktop` config fills in any flag left unset; CLI flags win.
    let (cfg, warns) = crate::config::desktop_config(&cwd);
    let cfg_base = cfg.base_dir.clone();
    for w in &warns {
        ui::warn(w);
    }
    apply_config_defaults(&mut a, cfg);

    // Fail fast on bad deep-link schemes or an unknown compress format, before
    // any packaging work.
    if let Err(e) = crate::deep_links::validate_schemes(&a.deep_links) {
        fail(&e);
    }
    if let Some(format) = &a.compress {
        if let Err(e) = crate::selfextract::validate_format(format) {
            fail(&e);
        }
    }

    // No entry: package the current directory (`inka desktop` == `inka desktop .`).
    let entry = a.entry.clone().unwrap_or_else(|| PathBuf::from("."));
    let app_name = a.app_name.clone().unwrap_or_else(|| {
        if entry.is_dir() {
            entry
                .canonicalize()
                .ok()
                .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
                .or_else(|| entry.file_name().map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "app".to_string())
        } else {
            entry
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "app".to_string())
        }
    });
    let app_name = sanitize_name(&app_name);
    let mut out = a.output.clone().unwrap_or_else(|| PathBuf::from(&app_name));

    // `-o App.msi` (like Deno): the MSI is the final artifact; the intermediate
    // app dir drops the extension. MSI packaging requires a Windows target.
    let mut msi_output: Option<PathBuf> = None;
    if out
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("msi"))
    {
        if !cfg!(windows) {
            fail("building a .msi requires a Windows target (x86_64-pc-windows-msvc)");
        }
        msi_output = Some(out.clone());
        out.set_extension("");
        if out.as_os_str().is_empty() {
            out = PathBuf::from(&app_name);
        }
    }

    let backend = a.backend.clone().unwrap_or_else(|| "webview".to_string());
    if !known_backend(&backend) {
        fail(&format!(
            "unknown backend '{backend}' (expected webview, cef, or raw)"
        ));
    }
    let id = match a.identifier.as_deref() {
        Some(id) => {
            validate_identifier(id);
            id.to_string()
        }
        None => format!("com.inka.desktop.{}", app_name.to_lowercase()),
    };

    // Dev mode (`--hmr`/`--inspect*`): run the source tree directly through the
    // shared runtime and the laufey backend (no packaging).
    let inspect_requested =
        a.inspect.is_some() || a.inspect_brk.is_some() || a.inspect_wait.is_some();
    if a.hmr || inspect_requested {
        let backend_path = resolve_backend(&backend);
        run_desktop_dev(&a, &entry, &app_name, &cfg_base, &backend_path);
    }

    ui::title("desktop");

    // Framework directory with no explicit icon: fall back to its favicon.
    if a.icon.is_none() && entry.is_dir() {
        if let Ok(Some(det)) = crate::framework::detect_framework(&entry) {
            if let Some(fav) = crate::framework::find_framework_favicon(&entry, &det) {
                ui::info(format!("using {} favicon as the app icon", det.name));
                a.icon = Some(IconArg::Single(fav));
            }
        }
    }

    // ---- payload ----
    // Built into a staging dir, then packed into the per-app `<App>.so` (so the
    // whole app is one file the backend loads and auto-update can patch).
    let staging = out.join(".inka-payload");
    let _ = fs::remove_dir_all(&staging);
    if let Err(e) = fs::create_dir_all(&staging) {
        fail(&format!("cannot create {}: {e}", staging.display()));
    }
    if let Some(src) = &a.payload {
        if !src.is_dir() {
            fail(&format!("--payload is not a directory: {}", src.display()));
        }
        if let Err(e) = copy_dir(src, &staging) {
            fail(&e);
        }
    } else if entry.is_dir() {
        if let Err(e) = build_framework(&entry, &staging, &a) {
            fail(&e);
        }
    } else {
        if !entry.is_file() {
            fail(&format!("entry not found: {}", entry.display()));
        }
        if let Err(e) = build_entry(&cwd, &entry, &staging, &a) {
            fail(&e);
        }
    }

    // ---- pack + backend + shim + marker + desktop entry ----
    let files = match collect_files(&staging) {
        Ok(f) => f,
        Err(e) => fail(&e),
    };
    let _ = fs::remove_dir_all(&staging);
    let mut manifest = format!("module=main.js\napp-name={app_name}\n");
    if let Some(v) = &a.app_version {
        manifest.push_str(&format!("version={v}\n"));
    }
    if let Some(v) = &a.release_base {
        manifest.push_str(&format!("release-base={v}\n"));
    }
    if let Some(v) = &a.error_reporting {
        manifest.push_str(&format!("error-reporting={v}\n"));
    }

    let backend_path = resolve_backend(&backend);
    let shim = resolve_shim();
    // `<App>.exe` on Windows, `<App>` on unix: laufey derives the backend
    // library name from its own stem (`<App>.dll` / `<App>.so`).
    let launcher = out.join(format!("{app_name}{}", platform::exe_suffix()));
    let runtime_so = out.join(format!("{app_name}{}", platform::runtime_lib_suffix()));

    // Clear a previous build (only one we generated), then stage the backend.
    // A CEF backend ships a whole directory (libcef/.dll + resources) whose
    // files must sit next to the launcher: laufey's RPATH is `.:$ORIGIN`.
    if let Err(e) = reserve_app_dir(&out) {
        fail(&e);
    }
    let _ = fs::write(out.join(APP_DIR_MARKER), b"");
    let (staged_backend, cef_bundled) = match stage_backend(&backend_path, &out) {
        Ok(v) => v,
        Err(e) => fail(&e),
    };
    if let Err(e) = check_target_collisions(&launcher, &runtime_so, &staged_backend) {
        fail(&e);
    }

    if let Err(e) = pack_shim(&shim, &runtime_so, &files, &manifest) {
        fail(&e);
    }
    if launcher != staged_backend {
        // Rename the backend to the app name so laufey auto-loads the co-located
        // `<App>.so`/`<App>.dll` shim.
        fs::rename(&staged_backend, &launcher).unwrap_or_else(|e| {
            fail(&format!(
                "cannot rename backend to {}: {e}",
                launcher.display()
            ))
        });
    }
    set_exec(&launcher);
    set_exec(&runtime_so);

    // Runtime tuple marker: the shim loads `libinka_runtime-<tuple>.{so,dll}`
    // from the inka data dir. Explicit env wins; otherwise use the newest
    // installed runtime (the release runtime is desktop-enabled). Without
    // either, the app needs `$INKA_DESKTOP_RUNTIME` at launch.
    let runtime_tuple = installed_runtime_tuple();
    match &runtime_tuple {
        Some(tuple) => {
            let _ = fs::write(out.join("runtime-version"), format!("{tuple}\n"));
        }
        None => ui::warn(
            "no installed inka runtime found; packaged apps need INKA_DESKTOP_RUNTIME until `inka update`",
        ),
    }

    if let Some(icon) = &a.icon {
        apply_app_icon(icon, &launcher, &out);
    }
    // A `.desktop` entry is a Linux desktop-integration file; Windows uses the
    // exe's embedded icon and (later) an MSI shortcut.
    #[cfg(unix)]
    let _ = fs::write(
        out.join(format!("{id}.desktop")),
        desktop_entry(&app_name, &id),
    );

    // Deep-link registration writes into the bundle (`.bat` on Windows,
    // `.desktop` MimeType/`%u` on Linux) before any self-extract transform, so
    // it ships inside the payload.
    if !a.deep_links.is_empty() {
        if let Err(e) = crate::deep_links::register(&out, &a.deep_links) {
            ui::warn(format!("could not register deep links: {e}"));
        }
    }

    // Optional self-extracting transform: replace the app dir with a thin dir
    // plus a compressed payload before the archive/MSI wrap it.
    if let Some(format) = &a.compress {
        if let Err(e) =
            crate::selfextract::make_self_extracting(&out, &app_name, &id, cfg!(windows))
        {
            fail(&format!("could not make the app self-extracting: {e}"));
        } else {
            ui::row("payload format", format);
        }
    }

    // ---- artifacts ----
    // Linux `--installer` builds a POSIX `install.sh` + tarball. Everything else
    // also emits the portable archive (`.zip` on Windows, `.tar.gz` on unix);
    // Windows additionally builds an `.msi` for `--installer` or `-o *.msi`.
    let installer_outputs = if a.installer && cfg!(unix) {
        let runtime = match runtime_tuple.as_deref() {
            Some(t) => t,
            None => fail(
                "cannot build an installer without the required runtime tuple; \
                 run `inka update` or set INKA_DESKTOP_RUNTIME_TUPLE",
            ),
        };
        let spec = crate::installer::Spec {
            app_name: &app_name,
            app_id: &id,
            backend: &backend,
            runtime,
            app_version: a.app_version.as_deref(),
            app_base: a.release_base.as_deref(),
            engine_base: a.engine_base.as_deref(),
        };
        match crate::installer::build(&out, &spec) {
            Ok(o) => Some(o),
            Err(e) => fail(&e),
        }
    } else {
        if let Err(e) = write_portable_archive(&out) {
            ui::warn(e);
        }
        None
    };

    // Windows MSI: from `-o App.msi` or `--installer`.
    if cfg!(windows) {
        if let Some(msi_path) = msi_output
            .clone()
            .or_else(|| a.installer.then(|| out.with_extension("msi")))
        {
            let icon = out.join("AppIcon.ico");
            let spec = crate::windows_msi::Spec {
                identifier: Some(&id),
                version: a.app_version.as_deref(),
                manufacturer: &app_name,
                icon: icon.is_file().then_some(icon.as_path()),
            };
            match crate::windows_msi::create(&out, &msi_path, &spec) {
                Ok(()) => ui::row("msi", msi_path.display()),
                Err(e) => ui::warn(format!("could not build {}: {e}", msi_path.display())),
            }
        }
    }

    ui::section("App");
    ui::row("name", &app_name);
    ui::row("launcher", launcher.display());
    ui::row("runtime", runtime_so.display());
    ui::row("payload", format!("{} file(s) embedded", files.len()));
    ui::row("backend", backend_path.display());
    if cef_bundled {
        ui::row("runtime files", cef_runtime_row());
    }
    ui::row("id", &id);
    if let Some(o) = &installer_outputs {
        ui::row("installer", o.script.display());
        ui::row("tarball", o.tarball.display());
        ui::row("sha256", o.sha256.display());
    }
    ui::status_ok(format!("packaged {}", out.display()));
}

#[cfg(windows)]
fn cef_runtime_row() -> &'static str {
    "laufey + shared CEF (copied)"
}

#[cfg(not(windows))]
fn cef_runtime_row() -> &'static str {
    "laufey + shared CEF (symlinked)"
}

/// Apply the configured icon to the packaged app. Windows embeds it into the
/// app exe (PE resources) and writes an `AppIcon.ico`; unix ships `AppIcon.png`.
///
/// Both branches are compiled on every platform (the PE path is pure Rust), so
/// the Linux build type-checks the Windows-only path; the runtime picks one.
fn apply_app_icon(icon: &IconArg, launcher: &Path, out: &Path) {
    if cfg!(windows) {
        windows_icon(icon, launcher, out);
    } else {
        let _ = launcher;
        unix_icon(icon, out);
    }
}

/// unix: copy the single icon, or the largest entry of a set, as `AppIcon.png`.
#[cfg_attr(windows, allow(dead_code))]
fn unix_icon(icon: &IconArg, out: &Path) {
    let src = match icon {
        IconArg::Single(p) => p.clone(),
        IconArg::Set(entries) => match entries.iter().max_by_key(|(_, s)| *s) {
            Some((p, _)) => p.clone(),
            None => return,
        },
    };
    if src.is_file() {
        let _ = fs::copy(&src, out.join("AppIcon.png"));
    } else {
        ui::warn(format!("icon not found: {}", src.display()));
    }
}

/// Windows: build/copy `AppIcon.ico` (from a set when given) and embed the icon
/// into the app exe via libsui, which generates the multi-resolution PE icon
/// resources itself. Compiled on every platform (see `apply_app_icon`).
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_icon(icon: &IconArg, launcher: &Path, out: &Path) {
    let ico_path = out.join("AppIcon.ico");
    let icon_bytes: Option<Vec<u8>> = match icon {
        IconArg::Set(entries) => {
            let mut read: Vec<(PathBuf, u32)> = Vec::new();
            for (path, size) in entries {
                if path.is_file() {
                    read.push((path.clone(), *size));
                } else {
                    ui::warn(format!("icon not found: {}", path.display()));
                }
            }
            match crate::ico::convert_icon_set_to_ico(&read, &ico_path) {
                Ok(()) => fs::read(&ico_path).ok(),
                Err(e) => {
                    ui::warn(format!("could not build {}: {e}", ico_path.display()));
                    None
                }
            }
        }
        IconArg::Single(path) => {
            if !path.is_file() {
                ui::warn(format!("icon not found: {}", path.display()));
                return;
            }
            if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("ico"))
            {
                let _ = fs::copy(path, &ico_path);
            } else {
                ui::warn(format!(
                    "icon {} is not .ico; embedding it but not writing AppIcon.ico",
                    path.display()
                ));
            }
            fs::read(path).ok()
        }
    };

    let Some(bytes) = icon_bytes else { return };
    let result = fs::read(launcher)
        .map_err(|e| e.to_string())
        .and_then(|image| crate::payload::set_icon(&image, &bytes))
        .and_then(|patched| fs::write(launcher, patched).map_err(|e| e.to_string()));
    if let Err(e) = result {
        ui::warn(format!("could not embed the app icon: {e}"));
    }
}

/// Package the runnable app dir as a portable archive: `<App>.zip` on Windows,
/// `<App>.tar.gz` on unix. Best-effort: warns (does not fail) on error, since
/// the app dir itself is still usable.
fn write_portable_archive(out: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        let zip_path = out.with_extension("zip");
        zip_dir(out, &zip_path).map_err(|e| {
            format!(
                "could not write {}: {e}; app dir is still usable",
                zip_path.display()
            )
        })?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let tarball = out.with_extension("tar.gz");
        let parent = match out.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let dir_name = out
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&tarball)
            .arg("-C")
            .arg(&parent)
            .arg(&dir_name)
            .status();
        match status {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => Err(format!("tar exited with {s}; app dir is still usable")),
            Err(e) => Err(format!("could not run tar: {e}")),
        }
    }
}

/// Zip the `src` directory (its own name becomes the top-level entry) into
/// `dest`, using `/` separators. Used for the Windows portable artifact.
///
/// Compiled on every platform (and unit-tested) so Windows-only regressions are
/// caught by the normal Linux build; only `write_portable_archive` calls it, and
/// only on Windows.
#[cfg_attr(not(windows), allow(dead_code))]
fn zip_dir(src: &Path, dest: &Path) -> Result<(), String> {
    use std::io::Write;

    fn walk(
        base: &Path,
        dir: &Path,
        zip: &mut zip::ZipWriter<fs::File>,
        opts: zip::write::SimpleFileOptions,
    ) -> Result<(), String> {
        for ent in fs::read_dir(dir)
            .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
            .flatten()
        {
            let p = ent.path();
            let rel = p
                .strip_prefix(base)
                .map_err(|_| "path outside the archive root".to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            if p.is_dir() {
                zip.add_directory(format!("{rel}/"), opts)
                    .map_err(|e| format!("zip {rel}: {e}"))?;
                walk(base, &p, zip, opts)?;
            } else if p.is_file() {
                zip.start_file(&rel, opts)
                    .map_err(|e| format!("zip {rel}: {e}"))?;
                let data = fs::read(&p).map_err(|e| format!("read {}: {e}", p.display()))?;
                zip.write_all(&data)
                    .map_err(|e| format!("zip {rel}: {e}"))?;
            }
        }
        Ok(())
    }

    let parent = match src.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let file =
        fs::File::create(dest).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    walk(parent, src, &mut zip, opts)?;
    zip.finish()
        .map_err(|e| format!("cannot finalize zip: {e}"))?;
    Ok(())
}

/// Collect a staging directory into archive entries (relative paths, `/`
/// separators).
fn collect_files(root: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Result<(), String> {
        for ent in fs::read_dir(dir).map_err(|e| e.to_string())?.flatten() {
            let p = ent.path();
            if p.is_dir() {
                walk(base, &p, out)?;
            } else if p.is_file() {
                let rel = p
                    .strip_prefix(base)
                    .map_err(|_| "path outside staging".to_string())?
                    .to_string_lossy()
                    .replace('\\', "/");
                let data = fs::read(&p).map_err(|e| format!("read {}: {e}", p.display()))?;
                out.push((rel, data));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    Ok(out)
}

/// Copy the shim and embed the app payload as the `inka` section, producing the
/// self-contained per-app `.so`/`.dll` the laufey backend loads.
fn pack_shim(
    shim: &Path,
    dest: &Path,
    files: &[(String, Vec<u8>)],
    manifest: &str,
) -> Result<(), String> {
    let image = fs::read(shim).map_err(|e| format!("cannot read {}: {e}", shim.display()))?;
    let payload = inka_format::encode_section_payload(files, manifest.as_bytes());
    let out = crate::payload::embed(&image, &payload)?;
    fs::write(dest, &out).map_err(|e| format!("cannot write {}: {e}", dest.display()))?;
    Ok(())
}

fn run_build(dir: &Path) -> Result<(), String> {
    let status = if dir.join("deno.json").is_file() || dir.join("deno.jsonc").is_file() {
        Command::new("deno")
            .args(["task", "build"])
            .current_dir(dir)
            .status()
    } else {
        Command::new("npm")
            .args(["run", "build"])
            .current_dir(dir)
            .status()
    };
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("the framework build failed ({s})")),
        Err(e) => Err(format!("could not run the framework build: {e}")),
    }
}

/// Framework mode (`inka desktop .`): detect a supported framework, run its
/// build, generate + bundle its entrypoint, and ship its build output. Detection
/// is vendored from Deno (`crate::framework`).
fn build_framework(dir: &Path, payload: &Path, a: &Args) -> Result<(), String> {
    // Absolute project root: the bundler resolves the generated entry against
    // its `cwd`, which must be absolute for a relative `entry` to resolve.
    let dir = &dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let detection = match crate::framework::detect_framework(dir)? {
        Some(d) => d,
        None => {
            return Err(format!(
                "could not detect a supported framework in {} (supported: Fresh, Astro, \
                 Remix, React Router, SvelteKit, Nuxt, SolidStart, TanStack Start, Vite); \
                 pass an explicit entry file instead",
                dir.display()
            ))
        }
    };
    if detection.name == "Next.js" {
        return Err(
            "Next.js packaging is not supported yet: its server can't be bundled into \
             inka's single-file desktop payload. Serve a `next build` output yourself and \
             pass an explicit entry, or use a supported framework."
                .to_string(),
        );
    }
    ui::info(format!("detected {}; running its build", detection.name));
    if detection.build {
        run_build(dir)?;
    }
    // Generate the entry inside the project so its relative imports resolve at
    // bundle time, then bundle it into payload/main.js.
    let entry_path = dir.join("inka-desktop-entry.js");
    fs::write(&entry_path, &detection.entrypoint_code)
        .map_err(|e| format!("cannot write {}: {e}", entry_path.display()))?;
    let result = build_entry(dir, &entry_path, payload, a);
    let _ = fs::remove_file(&entry_path);
    result?;
    // Ship the build output the generated server reads at runtime
    // (`import.meta.dirname`).
    for inc in &detection.include_paths {
        let src = dir.join(inc);
        if !src.is_dir() {
            return Err(format!(
                "the {} build did not produce {}",
                detection.name,
                src.display()
            ));
        }
        copy_dir(&src, &payload.join(inc))?;
    }
    Ok(())
}

/// Bundle the entry into `payload/main.js` (or copy it verbatim with
/// `--no-bundle`).
fn build_entry(_cwd: &Path, _entry: &Path, payload: &Path, a: &Args) -> Result<(), String> {
    if a.no_bundle {
        let dest = payload.join("main.js");
        fs::copy(_entry, &dest).map_err(|e| format!("cannot copy entry: {e}"))?;
        return Ok(());
    }
    #[cfg(feature = "bundle")]
    {
        let entry_rel = crate::embed::rel_from_cwd(_cwd, _entry)?;
        let bundle = inka_bundler::bundle(inka_bundler::BundleOptions {
            cwd: _cwd,
            entry: &entry_rel,
            external: &a.external,
            minify: a.minify,
            sourcemap: a.sourcemap,
            fetch: false,
        })?;
        for w in &bundle.warnings {
            ui::warn(w);
        }
        fs::write(payload.join("main.js"), bundle.code)
            .map_err(|e| format!("cannot write main.js: {e}"))?;
        Ok(())
    }
    #[cfg(not(feature = "bundle"))]
    {
        let _ = (payload, a);
        Err("inka was built without bundling support; rebuild with `--features bundle` or pass --no-bundle".to_string())
    }
}

#[cfg(unix)]
fn desktop_entry(app_name: &str, id: &str) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName={app_name}\nExec={app_name}\nIcon=AppIcon\nStartupWMClass={id}\nCategories=Utility;\n"
    )
}

fn set_exec(path: &Path) {
    let _ = platform::set_exec(path);
}

fn copy_dir(src: &Path, dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    for ent in fs::read_dir(src).map_err(|e| e.to_string())?.flatten() {
        let from = ent.path();
        let to = dest.join(ent.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            fs::copy(&from, &to).map_err(|e| format!("copy {}: {e}", from.display()))?;
        }
    }
    Ok(())
}

/// Stage the laufey backend into the app dir and return `(staged path, is_cef)`.
///
/// A CEF backend directory carries `libcef.so`/`libcef.dll` plus Chromium
/// resources (`*.pak`, `icudtl.dat`, `locales/`, …) that CEF resolves next to
/// the launcher; laufey's RPATH is `.:$ORIGIN`, so they must be reachable from
/// the app dir. Those files are shared per machine (see `crate::cef`) and
/// symlinked in on unix; if sharing is unavailable (always on Windows, or if
/// linking fails), fall back to a self-contained copy. Other backends link
/// system libraries and ship only the binary (`laufey_webview.exe` is
/// self-contained) — copy just that, so a dev override
/// (`INKA_LAUFEY_BACKEND`/`LAUFEY_DEV_DIR`) never drags in a whole
/// `target/release`.
fn stage_backend(backend_path: &Path, out: &Path) -> Result<(PathBuf, bool), String> {
    stage_backend_in(backend_path, out, None)
}

/// `stage_backend` with an explicit shared CEF dir (used by tests); `None`
/// resolves the per-machine `INKA_CEF_HOME`/XDG location.
fn stage_backend_in(
    backend_path: &Path,
    out: &Path,
    shared_override: Option<&Path>,
) -> Result<(PathBuf, bool), String> {
    fs::create_dir_all(out).map_err(|e| format!("cannot create {}: {e}", out.display()))?;
    let dir = backend_path.parent().ok_or_else(|| {
        format!(
            "backend has no parent directory: {}",
            backend_path.display()
        )
    })?;
    let exe_name = backend_path
        .file_name()
        .ok_or_else(|| format!("backend has no file name: {}", backend_path.display()))?
        .to_os_string();
    let dest = out.join(&exe_name);
    let cef = dir.join(platform::cef_lib_name()).is_file();
    if !cef {
        fs::copy(backend_path, &dest)
            .map_err(|e| format!("cannot place backend {}: {e}", backend_path.display()))?;
        return Ok((dest, false));
    }

    // Prefer the shared runtime: symlink it in so each app is a few MB.
    let shared = match shared_override {
        Some(s) => crate::cef::ensure_shared_cef_at(dir, backend_path, s).map(|()| s.to_path_buf()),
        None => crate::cef::ensure_shared_cef(dir, backend_path),
    };
    let linked = shared.and_then(|shared| {
        crate::cef::link_into_app(&shared, out)?;
        fs::copy(backend_path, &dest)
            .map_err(|e| format!("cannot place backend {}: {e}", backend_path.display()))?;
        Ok(shared)
    });
    match linked {
        Ok(_) => Ok((dest, true)),
        Err(e) => {
            ui::warn(format!(
                "could not use the shared CEF runtime ({e}); copying it into the app"
            ));
            // Discard any partial links and ship a self-contained copy.
            let _ = fs::remove_dir_all(out);
            fs::create_dir_all(out).map_err(|e| format!("cannot create {}: {e}", out.display()))?;
            let _ = fs::write(out.join(APP_DIR_MARKER), b"");
            fs::copy(backend_path, &dest)
                .map_err(|e| format!("cannot place backend {}: {e}", backend_path.display()))?;
            crate::cef::copy_dir_all(dir, out)?;
            // Drop our download marker and laufey's self-extracting runtime
            // cache if they tagged along from the backend cache dir.
            let _ = fs::remove_file(out.join(".downloaded"));
            if let Some(stem) = Path::new(&exe_name).file_stem() {
                let stem = stem.to_string_lossy();
                let _ = fs::remove_dir_all(out.join(format!(".{stem}")));
                let _ = fs::remove_file(out.join(format!(".{stem}.cache")));
            }
            Ok((dest, true))
        }
    }
}

/// Prepare `out` for a fresh package. A directory we generated (it carries
/// `APP_DIR_MARKER`) or an empty one is cleared; anything else is treated as
/// user data and refused rather than silently deleted.
fn reserve_app_dir(out: &Path) -> Result<(), String> {
    match fs::symlink_metadata(out) {
        Err(_) => {}
        Ok(md) if md.is_dir() => {
            // `runtime-version` lets a package built before the marker existed
            // be replaced in place instead of being mistaken for user data.
            let is_ours =
                out.join(APP_DIR_MARKER).exists() || out.join("runtime-version").is_file();
            let is_empty = fs::read_dir(out)
                .map(|mut e| e.next().is_none())
                .unwrap_or(false);
            if !is_ours && !is_empty {
                return Err(format!(
                    "refusing to overwrite {}: it was not created by `inka desktop`; \
                     pass -o/--output to choose a different directory",
                    out.display()
                ));
            }
            fs::remove_dir_all(out).map_err(|e| format!("cannot clear {}: {e}", out.display()))?;
        }
        Ok(_) => {
            return Err(format!(
                "refusing to overwrite {}: a file with that name exists; pass -o/--output",
                out.display()
            ))
        }
    }
    fs::create_dir_all(out).map_err(|e| format!("cannot create {}: {e}", out.display()))
}

/// Refuse app-derived paths that would clobber a staged backend file. The app
/// dir starts as a copy of the backend dir, and `fs::copy`/`fs::rename` replace
/// silently, so an app named e.g. `libcef` would overwrite `libcef.so`.
fn check_target_collisions(
    launcher: &Path,
    runtime_so: &Path,
    staged_backend: &Path,
) -> Result<(), String> {
    if runtime_so.exists() {
        return Err(format!(
            "app would overwrite a backend file at {}; pass --name/-o to rename the app",
            runtime_so.display()
        ));
    }
    // The staged backend is what we rename *into* the launcher, so it colliding
    // with itself is the normal case, not a clash.
    if launcher != staged_backend && launcher.exists() {
        return Err(format!(
            "app would overwrite a backend file at {}; pass --name/-o to rename the app",
            launcher.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    /// A fresh, empty scratch directory under the temp dir.
    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "inka-desktop-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn embedded_payload_section_roundtrips() {
        // Embed into a small system ELF (falling back to the test binary), then
        // assert the original image prefix is preserved and the payload is
        // grafted verbatim. The runtime read path (`locate_section`) is
        // exercised by the desktop e2e battery once a runtime is present.
        let image = ["/bin/true", "/usr/bin/true", "/bin/echo"]
            .iter()
            .find_map(|p| fs::read(p).ok())
            .or_else(|| fs::read("/proc/self/exe").ok())
            .expect("a small ELF to embed into");
        let files = vec![("main.js".to_string(), b"console.log(1)".to_vec())];
        let manifest = "module=main.js\napp-name=Demo\n";
        let payload = inka_format::encode_section_payload(&files, manifest.as_bytes());
        let out = crate::payload::embed(&image, &payload).expect("embed payload section");
        assert_eq!(
            inka_format::locate_section(&out).expect("locate embedded section"),
            payload.as_slice()
        );
        assert_eq!(&out[..4], &image[..4], "ELF magic must be preserved");
        assert!(out.len() > image.len(), "image must grow");
        assert!(
            out.windows(payload.len()).any(|w| w == payload),
            "payload bytes must appear in the image"
        );
        let decoded = inka_format::read_section_payload(&payload).unwrap();
        assert_eq!(decoded.manifest, manifest.as_bytes());
        assert_eq!(inka_format::parse_archive(decoded.archive).unwrap(), files);
    }

    #[test]
    fn zip_dir_packages_the_app_tree() {
        use std::io::Read;
        let base = scratch("zipdir");
        let app = base.join("MyApp");
        fs::create_dir_all(app.join("locales")).unwrap();
        fs::write(app.join("MyApp.exe"), b"exe").unwrap();
        fs::write(app.join("MyApp.dll"), b"shim").unwrap();
        fs::write(app.join("locales/en-US.pak"), b"pak").unwrap();
        let dest = base.join("MyApp.zip");
        zip_dir(&app, &dest).unwrap();

        let f = fs::File::open(&dest).unwrap();
        let mut zip = zip::ZipArchive::new(f).unwrap();
        let mut names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "MyApp/MyApp.dll",
                "MyApp/MyApp.exe",
                "MyApp/locales/",
                "MyApp/locales/en-US.pak",
            ]
        );
        let mut exe = zip.by_name("MyApp/MyApp.exe").unwrap();
        let mut buf = Vec::new();
        exe.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"exe");
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn stage_backend_shares_cef_runtime_and_keeps_launcher_real() {
        let base = scratch("stage-cef");
        let backend_dir = base.join("cef");
        let shared = base.join("shared/cef");
        let out = base.join("app");
        fs::create_dir_all(backend_dir.join("locales")).unwrap();
        fs::write(backend_dir.join("laufey"), b"exe").unwrap();
        fs::write(backend_dir.join("libcef.so"), b"cef").unwrap();
        fs::write(backend_dir.join("chrome-sandbox"), b"sandbox").unwrap();
        fs::write(backend_dir.join("locales/en-US.pak"), b"pak").unwrap();
        fs::write(backend_dir.join(".downloaded"), b"v\n").unwrap();

        let (staged, cef) =
            stage_backend_in(&backend_dir.join("laufey"), &out, Some(&shared)).unwrap();

        assert!(cef, "a libcef.so sibling marks the CEF backend");
        assert_eq!(staged, out.join("laufey"));
        // The launcher stays a real per-app file (laufey derives `<exe>.so`).
        assert!(!fs::symlink_metadata(out.join("laufey"))
            .unwrap()
            .file_type()
            .is_symlink());
        // The runtime is shared once and symlinked into the app.
        assert!(shared.join("libcef.so").is_file());
        assert!(fs::symlink_metadata(out.join("libcef.so"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_link(out.join("libcef.so")).unwrap(),
            shared.join("libcef.so")
        );
        assert!(out.join("locales/en-US.pak").is_file());
        assert!(!out.join(".downloaded").exists(), "download marker dropped");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn stage_backend_without_cef_copies_only_the_binary() {
        let base = scratch("stage-webview");
        let backend_dir = base.join("webview");
        let out = base.join("app");
        fs::create_dir_all(&backend_dir).unwrap();
        fs::write(backend_dir.join("laufey_webview"), b"exe").unwrap();
        fs::write(backend_dir.join("extra.txt"), b"extra").unwrap();

        let (staged, cef) = stage_backend(&backend_dir.join("laufey_webview"), &out).unwrap();

        assert!(!cef);
        assert_eq!(staged, out.join("laufey_webview"));
        assert!(out.join("laufey_webview").is_file());
        assert!(
            !out.join("extra.txt").exists(),
            "non-CEF backends copy the binary only"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn reserve_app_dir_clears_ours_and_empty_but_refuses_user_data() {
        let base = scratch("reserve");

        let fresh = base.join("fresh");
        reserve_app_dir(&fresh).unwrap();
        assert!(fresh.is_dir(), "missing dir must be created");

        let empty = base.join("empty");
        fs::create_dir_all(&empty).unwrap();
        reserve_app_dir(&empty).unwrap();
        assert!(empty.is_dir());

        let ours = base.join("ours");
        fs::create_dir_all(&ours).unwrap();
        fs::write(ours.join(APP_DIR_MARKER), b"").unwrap();
        fs::write(ours.join("stale"), b"x").unwrap();
        reserve_app_dir(&ours).unwrap();
        assert!(!ours.join("stale").exists(), "marker dir must be cleared");

        let user = base.join("user");
        fs::create_dir_all(&user).unwrap();
        fs::write(user.join("important"), b"keep").unwrap();
        assert!(reserve_app_dir(&user).is_err());
        assert!(user.join("important").is_file(), "user data preserved");

        let file = base.join("afile");
        fs::write(&file, b"x").unwrap();
        assert!(reserve_app_dir(&file).is_err());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn check_target_collisions_rejects_clobbering_backend_files() {
        let base = scratch("collide");
        let out = base.join("app");
        fs::create_dir_all(&out).unwrap();
        let libcef = out.join("libcef.so");
        fs::write(&libcef, b"cef").unwrap();

        // Runtime `.so` landing on the backend library.
        assert!(
            check_target_collisions(&out.join("libcef"), &libcef, &libcef).is_err(),
            "app named libcef must be refused"
        );

        // Launcher landing on another backend file.
        fs::write(out.join("laufey"), b"exe").unwrap();
        assert!(
            check_target_collisions(&out.join("laufey"), &out.join("app.so"), &libcef).is_err()
        );

        // Normal names are fine, and renaming the backend onto itself is fine.
        assert!(
            check_target_collisions(&out.join("myapp"), &out.join("myapp.so"), &libcef).is_ok()
        );
        assert!(check_target_collisions(
            &out.join("laufey"),
            &out.join("myapp.so"),
            &out.join("laufey")
        )
        .is_ok());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_dev_command_prefers_explicit_then_package_json_then_deno() {
        let dir = std::env::temp_dir().join(format!("inka-devcmd-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Explicit flag wins and is whitespace-split.
        assert_eq!(
            resolve_dev_command(&dir, Some("vite dev --host")).unwrap(),
            vec!["vite", "dev", "--host"]
        );

        // Nothing configured -> an actionable error.
        assert!(resolve_dev_command(&dir, None).is_err());

        // package.json `scripts.dev` uses the lockfile's package manager.
        fs::write(dir.join("package.json"), r#"{"scripts":{"dev":"vite"}}"#).unwrap();
        fs::write(dir.join("yarn.lock"), "").unwrap();
        assert_eq!(
            resolve_dev_command(&dir, None).unwrap(),
            vec!["yarn", "run", "dev"]
        );

        // Fall back to `deno task dev` when only deno.json has the task.
        fs::remove_file(dir.join("package.json")).unwrap();
        fs::remove_file(dir.join("yarn.lock")).unwrap();
        fs::write(
            dir.join("deno.json"),
            r#"{"tasks":{"dev":"deno run -A dev.ts"}}"#,
        )
        .unwrap();
        assert_eq!(
            resolve_dev_command(&dir, None).unwrap(),
            vec!["deno", "task", "dev"]
        );

        let _ = fs::remove_dir_all(&dir);
    }
}

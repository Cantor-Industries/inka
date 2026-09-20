// inka desktop: package a web app into a desktop application that shares the
// machine's inka runtime.
//
// Layout produced (Linux):
//
//   <App>/
//     <App>            laufey backend (window + system webview), renamed
//     <App>.so         per-app shim (loads the shared libinka_runtime)
//     app/             bundled app payload (main.js + assets)
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
use crate::ui;

/// laufey backend release inka is pinned to (matches `laufey = 0.7.0`).
pub(crate) const LAUFEY_VERSION: &str = "0.7.0";
/// Linux target triple for laufey backend assets.
pub(crate) const LAUFEY_TARGET: &str = "x86_64-unknown-linux-gnu";
/// Marker written at the root of an app dir we generated, so a later package
/// build may safely clear it (and nothing else).
const APP_DIR_MARKER: &str = ".inka-desktop-app";

/// Pinned SHA-256 digests for laufey backend archives (trust anchor). Kept in
/// sync with `denoland/deno`'s `cli/laufey_sums.lock` for the pinned release, so
/// a download is verified against a value checked into this repo rather than
/// the release's own (unsigned) `SHA256SUMS`.
const LAUFEY_SUMS: &[(&str, &str)] = &[
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

fn laufey_archive_name(backend: &str) -> String {
    let archive_backend = if backend == "raw" { "winit" } else { backend };
    let ext = if LAUFEY_TARGET.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    format!("laufey-{archive_backend}-{LAUFEY_TARGET}.{ext}")
}

fn laufey_release_base() -> String {
    format!("https://github.com/littledivy/laufey/releases/download/v{LAUFEY_VERSION}")
}

/// Inka's own laufey cache root (`$XDG_CACHE_HOME/inka/laufey`, else
/// `~/.cache/inka/laufey`).
fn inka_laufey_cache() -> Option<PathBuf> {
    if let Some(x) = env::var_os("XDG_CACHE_HOME") {
        let p = PathBuf::from(x);
        if p.is_absolute() {
            return Some(p.join("inka/laufey"));
        }
    }
    env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/inka/laufey"))
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

struct Args {
    entry: Option<PathBuf>,
    output: Option<PathBuf>,
    app_name: Option<String>,
    identifier: Option<String>,
    backend: Option<String>,
    icon: Option<PathBuf>,
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
            "--icon" => a.icon = Some(PathBuf::from(next(&mut it, arg))),
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

/// The laufey backend executable name for a backend kind.
fn backend_exe(backend: &str) -> &'static str {
    match backend {
        "cef" => "laufey",
        "webview" => "laufey_webview",
        // `raw` ships upstream as `winit`.
        _ => "laufey_winit",
    }
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
fn download_laufey(backend: &str) -> Result<PathBuf, String> {
    if LAUFEY_TARGET.contains("windows") {
        return Err("Windows backends are not supported yet".to_string());
    }
    let cache = inka_laufey_cache()
        .ok_or_else(|| "cannot determine a cache directory (set HOME)".to_string())?;
    let dir = cache.join(LAUFEY_VERSION).join(backend).join(LAUFEY_TARGET);
    let exe = backend_exe(backend);
    if dir.join(".downloaded").is_file() {
        if let Some(p) = find_file(&dir, exe) {
            return Ok(p);
        }
    }
    let archive = laufey_archive_name(backend);
    let expected = LAUFEY_SUMS
        .iter()
        .find(|(n, _)| *n == archive)
        .map(|(_, h)| *h)
        .ok_or_else(|| format!("no pinned checksum for {archive}"))?;
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let tmp = dir.join(format!(".{archive}.tmp{}", std::process::id()));
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
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&tmp)
        .args(["--no-same-owner", "--no-same-permissions", "-C"])
        .arg(&dir)
        .status()
        .map_err(|e| format!("failed to run tar: {e}"))?;
    let _ = fs::remove_file(&tmp);
    if !status.success() {
        return Err(format!("tar extraction failed for {archive}"));
    }
    let found = find_file(&dir, exe)
        .ok_or_else(|| format!("'{exe}' not found in the {archive} archive"))?;
    let _ = fs::write(dir.join(".downloaded"), format!("v{LAUFEY_VERSION}\n"));
    Ok(found)
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
            dir.join(exe),
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
            .join(LAUFEY_TARGET)
            .join(exe);
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
            for name in ["libinka_desktop_shim.so", "inka-desktop-shim"] {
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
        a.output = cfg.output_linux.map(|o| base.join(o));
    }
    if a.icon.is_none() {
        a.icon = cfg.icon_linux.map(|i| base.join(i));
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

/// `inka desktop --hmr` / `--inspect*`: run `entry` (a source tree) through the
/// shared desktop runtime and the laufey backend, without packaging. Optionally
/// watches `base` for HMR and/or fronts the runtime inspector with the CDP
/// mux. Never returns.
fn run_desktop_dev(a: &Args, entry: &Path, app_name: &str, base: &Path, backend: &Path) -> ! {
    if entry.is_dir() {
        fail(
            "dev mode runs an entry file today; framework dev-server HMR is not wired yet \
             (pass an entry, e.g. `inka desktop --hmr main.ts`)",
        );
    }
    let entry_abs = entry
        .canonicalize()
        .unwrap_or_else(|e| fail(&format!("cannot resolve {}: {e}", entry.display())));
    let base_abs = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    let entry_rel = entry_abs
        .strip_prefix(&base_abs)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| {
            fail(&format!(
                "entry {} is outside the project dir {}",
                entry_abs.display(),
                base_abs.display()
            ))
        });
    let runtime = resolve_desktop_runtime();

    ui::title("desktop dev");
    ui::row("entry", &entry_rel);
    if a.hmr {
        ui::row("watch", base_abs.display());
    }
    ui::row("runtime", runtime.display());
    ui::row("backend", backend.display());

    let mut cmd = Command::new(backend);
    cmd.env("LAUFEY_RUNTIME_PATH", &runtime)
        .env("INKA_DESKTOP_PAYLOAD", &base_abs)
        .env("INKA_DESKTOP_ENTRY", &entry_rel)
        .env("INKA_DESKTOP_APP_NAME", app_name);
    if a.hmr {
        cmd.env("INKA_DESKTOP_HMR_DIR", &base_abs);
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

    let entry = a.entry.clone().unwrap_or_else(|| {
        ui::log_error("no entry file given");
        ui::hint("pass an entry (e.g. `inka desktop main.ts`)");
        usage()
    });
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
    let out = a.output.clone().unwrap_or_else(|| PathBuf::from(&app_name));

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
                a.icon = Some(fav);
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
    let launcher = out.join(&app_name);
    let runtime_so = out.join(format!("{app_name}.so"));

    // Clear a previous build (only one we generated), then stage the backend.
    // A CEF backend ships a whole directory (libcef.so + resources) whose files
    // must sit next to the launcher: laufey's RPATH is `.:$ORIGIN`.
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
        // Rename the backend to the app name so laufey auto-loads `<App>.so`.
        fs::rename(&staged_backend, &launcher).unwrap_or_else(|e| {
            fail(&format!(
                "cannot rename backend to {}: {e}",
                launcher.display()
            ))
        });
    }
    set_exec(&launcher);
    set_exec(&runtime_so);

    // Runtime tuple marker: the shim loads `libinka_runtime-<tuple>.so` from the
    // inka data dir. Explicit env wins; otherwise use the newest installed
    // runtime (the release runtime is desktop-enabled). Without either, the app
    // needs `$INKA_DESKTOP_RUNTIME` at launch.
    match installed_runtime_tuple() {
        Some(tuple) => {
            let _ = fs::write(out.join("runtime-version"), format!("{tuple}\n"));
        }
        None => ui::warn(
            "no installed inka runtime found; packaged apps need INKA_DESKTOP_RUNTIME until `inka update`",
        ),
    }

    if let Some(icon) = &a.icon {
        if icon.is_file() {
            let _ = fs::copy(icon, out.join("AppIcon.png"));
        } else {
            ui::warn(format!("icon not found: {}", icon.display()));
        }
    }
    let _ = fs::write(
        out.join(format!("{id}.desktop")),
        desktop_entry(&app_name, &id),
    );

    // ---- tarball ----
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
        Ok(s) if s.success() => {}
        Ok(s) => ui::warn(format!("tar exited with {s}; app dir is still usable")),
        Err(e) => ui::warn(format!("could not run tar: {e}")),
    }

    ui::section("App");
    ui::row("name", &app_name);
    ui::row("launcher", launcher.display());
    ui::row("runtime", runtime_so.display());
    ui::row("payload", format!("{} file(s) embedded", files.len()));
    ui::row("backend", backend_path.display());
    if cef_bundled {
        ui::row("runtime files", "laufey + shared CEF (symlinked)");
    }
    ui::row("id", &id);
    ui::status_ok(format!("packaged {}", out.display()));
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

/// Copy the shim and append `[archive][manifest][footer]`, producing the
/// self-contained per-app `.so` the laufey backend loads.
fn pack_shim(
    shim: &Path,
    dest: &Path,
    files: &[(String, Vec<u8>)],
    manifest: &str,
) -> Result<(), String> {
    let mut out = fs::read(shim).map_err(|e| format!("cannot read {}: {e}", shim.display()))?;
    let archive = inka_format::encode_archive(files);
    out.extend_from_slice(&archive);
    out.extend_from_slice(manifest.as_bytes());
    out.extend_from_slice(&inka_format::encode_footer(
        archive.len() as u64,
        manifest.len() as u64,
    ));
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

fn desktop_entry(app_name: &str, id: &str) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName={app_name}\nExec={app_name}\nIcon=AppIcon\nStartupWMClass={id}\nCategories=Utility;\n"
    )
}

fn set_exec(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
    }
    #[cfg(not(unix))]
    let _ = path;
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
/// A CEF backend directory carries `libcef.so` plus Chromium resources
/// (`*.pak`, `icudtl.dat`, `locales/`, …) that CEF resolves next to the
/// launcher; laufey's RPATH is `.:$ORIGIN`, so they must be reachable from the
/// app dir. Those files are shared per machine (see `crate::cef`) and symlinked
/// in; if sharing is unavailable, fall back to a self-contained copy. Other
/// backends link system libraries and ship only the binary — copy just that, so
/// a dev override (`INKA_LAUFEY_BACKEND`/`LAUFEY_DEV_DIR`) never drags in a
/// whole `target/release`.
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
    let cef = dir.join("libcef.so").is_file();
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
}

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
const LAUFEY_VERSION: &str = "0.7.0";
/// Linux target triple for laufey backend assets.
const LAUFEY_TARGET: &str = "x86_64-unknown-linux-gnu";

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
/// when set, else the newest installed runtime.
fn installed_runtime_tuple() -> Option<String> {
    if let Ok(t) = env::var("INKA_DESKTOP_RUNTIME_TUPLE") {
        let t = t.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    crate::installed_parts_all(&crate::runtime_search_dirs())
        .last()
        .map(|(v, _)| v.to_string())
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
/// `output`/`icon` paths are project-relative, so they are joined with `cwd`.
fn apply_config_defaults(a: &mut Args, cwd: &Path, cfg: crate::config::DesktopConfig) {
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
        a.output = cfg.output_linux.map(|o| cwd.join(o));
    }
    if a.icon.is_none() {
        a.icon = cfg.icon_linux.map(|i| cwd.join(i));
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

pub fn cmd_desktop(args: &[String]) {
    let mut a = parse_args(args);
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // `deno.json` `desktop` config fills in any flag left unset; CLI flags win.
    let (cfg, warns) = crate::config::desktop_config(&cwd);
    for w in &warns {
        ui::warn(w);
    }
    apply_config_defaults(&mut a, &cwd, cfg);

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

    ui::title("desktop");

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
        if let Err(e) = build_framework(&entry, &staging) {
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

    fs::create_dir_all(&out)
        .unwrap_or_else(|e| fail(&format!("cannot create {}: {e}", out.display())));
    if let Err(e) = pack_shim(&shim, &runtime_so, &files, &manifest) {
        fail(&e);
    }
    fs::copy(&backend_path, &launcher)
        .unwrap_or_else(|e| fail(&format!("cannot place backend: {e}")));
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

/// The static server generated for framework apps (Vite for now). Serves the
/// build output from `./dist/` relative to the entry module.
const FRAMEWORK_MAIN_JS: &str = r#"// Generated by `inka desktop`.
const DIST = new URL("./dist/", import.meta.url);
const TYPES = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".jpg": "image/jpeg",
  ".jpeg": "image/jpeg",
  ".gif": "image/gif",
  ".ico": "image/x-icon",
  ".woff": "font/woff",
  ".woff2": "font/woff2",
  ".map": "application/json; charset=utf-8",
};
const read = async (u) => {
  try {
    return await Deno.readFile(u);
  } catch {
    return null;
  }
};
Deno.serve(async (req) => {
  const url = new URL(req.url);
  let path = decodeURIComponent(url.pathname);
  if (path.endsWith("/")) path += "index.html";
  const target = new URL("." + path, DIST);
  // Keep the resolved path inside DIST (reject `..` traversal).
  if (!target.href.startsWith(DIST.href)) {
    return new Response("not found", { status: 404 });
  }
  let body = await read(target);
  if (body === null) body = await read(new URL("index.html", DIST));
  if (body === null) return new Response("not found", { status: 404 });
  const dot = path.lastIndexOf(".");
  const type = dot >= 0 ? TYPES[path.slice(dot).toLowerCase()] : undefined;
  return new Response(body, {
    headers: { "content-type": type ?? "application/octet-stream" },
  });
});
"#;

/// True when `dir` looks like a Vite project.
fn looks_like_vite(dir: &Path) -> bool {
    for cfg in [
        "vite.config.js",
        "vite.config.ts",
        "vite.config.mjs",
        "vite.config.mts",
        "vite.config.cjs",
        "vite.config.cts",
    ] {
        if dir.join(cfg).is_file() {
            return true;
        }
    }
    if let Ok(text) = fs::read_to_string(dir.join("package.json")) {
        if text.contains("\"vite\"") {
            return true;
        }
    }
    false
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

/// Framework mode (`inka desktop .`): build a supported frontend and lay out a
/// static-server payload. Vite only for now (full deno parity is a follow-up).
fn build_framework(dir: &Path, payload: &Path) -> Result<(), String> {
    if !looks_like_vite(dir) {
        return Err(format!(
            "could not detect a supported framework in {} (only Vite is supported today); \
             pass an explicit entry file instead",
            dir.display()
        ));
    }
    ui::info("detected Vite; running its build");
    run_build(dir)?;
    let dist = dir.join("dist");
    if !dist.is_dir() {
        return Err(format!("the Vite build did not produce {}", dist.display()));
    }
    fs::create_dir_all(payload).map_err(|e| format!("cannot create {}: {e}", payload.display()))?;
    copy_dir(&dist, &payload.join("dist"))?;
    fs::write(payload.join("main.js"), FRAMEWORK_MAIN_JS)
        .map_err(|e| format!("cannot write main.js: {e}"))?;
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

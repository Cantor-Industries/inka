use std::env;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fs;
use std::path::{Path, PathBuf};

const FOOTER_LEN: usize = 24;
const MAGIC: &[u8] = b"INKFOOT5"; // bundle + embedded files

/// Per-tree marker recording the pid that created it, so a later run can reap
/// trees orphaned by a process that died without cleanup.
const OWNER_MARKER: &str = ".inka-owner";
/// Legacy marker-less trees are only reaped once they are this old.
const STALE_LEGACY_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Remove the extracted tree if the process exits from inside the runtime
/// (e.g. `Deno.exit`), which calls `exit()` and skips Rust destructors. The
/// `atexit` handler runs on that path; the `TempTree` `Drop` covers normal
/// returns. Declared `extern` to avoid a `libc` dependency.
#[cfg(unix)]
mod tree_cleanup {
    use std::path::Path;
    use std::sync::{Once, OnceLock};

    static ROOT: OnceLock<String> = OnceLock::new();
    static ARM: Once = Once::new();

    extern "C" fn cleanup() {
        if let Some(root) = ROOT.get() {
            let _ = std::fs::remove_dir_all(root);
        }
    }

    extern "C" {
        fn atexit(cb: extern "C" fn()) -> std::ffi::c_int;
    }

    pub(super) fn arm(root: &Path) {
        let _ = ROOT.set(root.to_string_lossy().into_owned());
        ARM.call_once(|| unsafe {
            atexit(cleanup);
        });
    }
}

#[cfg(not(unix))]
mod tree_cleanup {
    use std::path::Path;
    pub(super) fn arm(_root: &Path) {}
}

macro_rules! debug_log {
    ($($arg:tt)*) => {
        if std::env::var_os("INKA_DEBUG").is_some() {
            eprintln!($($arg)*);
        }
    };
}

/// Load the runtime with `RTLD_GLOBAL`. Native `.node` addons are `dlopen`ed
/// later by the runtime's `op_napi_open`; they resolve N-API/uv symbols from the
/// global scope, which `RTLD_LOCAL` (the `Library::new` default) hides — the
/// addon then aborts with `undefined symbol: napi_module_register`.
fn load_runtime_library(path: &Path) -> Result<libloading::Library, libloading::Error> {
    #[cfg(unix)]
    {
        use libloading::os::unix::{Library as UnixLibrary, RTLD_GLOBAL, RTLD_LAZY};
        unsafe {
            UnixLibrary::open(Some(path), RTLD_LAZY | RTLD_GLOBAL).map(libloading::Library::from)
        }
    }
    #[cfg(not(unix))]
    {
        unsafe { libloading::Library::new(path) }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Version(u64, u64, u64);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

fn parse_version(s: &str) -> Option<Version> {
    let s = s.trim();
    let mut parts = s.split('.');
    let a = parts.next()?.trim().parse().ok()?;
    let b = parts.next().unwrap_or("0").trim().parse().ok()?;
    let c = parts.next().unwrap_or("0").trim().parse().ok()?;
    Some(Version(a, b, c))
}

enum Trailer<'a> {
    /// An `INKFOOT5` archive of relative-path files + manifest.
    Archive {
        files: Vec<(String, Vec<u8>)>,
        manifest: &'a [u8],
    },
}

fn parse_trailer(bytes: &[u8]) -> Result<Trailer<'_>, String> {
    if bytes.len() < FOOTER_LEN {
        return Err("file smaller than footer".into());
    }
    let footer = &bytes[bytes.len() - FOOTER_LEN..];
    let magic = &footer[0..8];
    let alen = u64::from_le_bytes(footer[8..16].try_into().unwrap()) as usize;
    let mlen = u64::from_le_bytes(footer[16..24].try_into().unwrap()) as usize;
    if alen.saturating_add(mlen).saturating_add(FOOTER_LEN) > bytes.len() {
        return Err("trailer lengths out of range".into());
    }
    let mstart = bytes.len() - FOOTER_LEN - mlen;
    let pstart = mstart - alen;
    let manifest = &bytes[mstart..mstart + mlen];
    if magic != MAGIC {
        return Err("trailer magic not found (not an inka artifact?)".into());
    }
    let files = parse_archive(&bytes[pstart..mstart])?;
    Ok(Trailer::Archive { files, manifest })
}

/// Archive layout: repeated `{path_len u64}{data_len u64}{path}{data}`.
fn parse_archive(blob: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files = Vec::new();
    let mut rest = blob;
    while !rest.is_empty() {
        if rest.len() < 16 {
            return Err("malformed archive entry header".into());
        }
        let path_len = usize::try_from(u64::from_le_bytes(rest[0..8].try_into().unwrap()))
            .map_err(|_| "archive entry path length out of range".to_string())?;
        let data_len = usize::try_from(u64::from_le_bytes(rest[8..16].try_into().unwrap()))
            .map_err(|_| "archive entry data length out of range".to_string())?;
        rest = &rest[16..];
        let end = path_len
            .checked_add(data_len)
            .ok_or_else(|| "archive entry lengths overflow".to_string())?;
        if path_len == 0 || end > rest.len() {
            return Err("malformed archive entry lengths".into());
        }
        let path_bytes = &rest[..path_len];
        let path = std::str::from_utf8(path_bytes)
            .map_err(|_| "archive entry path is not valid UTF-8".to_string())?;
        validate_rel_path(path)?;
        let data = rest[path_len..end].to_vec();
        rest = &rest[end..];
        files.push((path.to_string(), data));
    }
    Ok(files)
}

fn validate_rel_path(path: &str) -> Result<(), String> {
    if path.is_empty() || Path::new(path).is_absolute() {
        return Err(format!("invalid archive path '{path}' (must be relative)"));
    }
    if path.contains('\0') {
        return Err("archive path contains a NUL byte".into());
    }
    for comp in path.split('/') {
        if comp == ".." {
            return Err(format!("archive path '{path}' escapes the artifact tree"));
        }
    }
    Ok(())
}

/// A freshly created, exclusive temp tree, removed when dropped.
struct TempTree {
    root: PathBuf,
}

impl TempTree {
    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Hex string from `/dev/urandom` (fallback: pid + time), for unpredictable
/// temp names. No new dependency; the launcher already assumes Linux.
fn random_hex(bytes: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; bytes];
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        if f.read_exact(&mut buf).is_ok() {
            let mut s = String::with_capacity(bytes * 2);
            for b in buf {
                s.push_str(&format!("{b:02x}"));
            }
            return s;
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}{:x}", std::process::id(), nanos)
}

/// Remove stale `inka-*` trees under the system temp dir left by processes that
/// died without running their cleanup (SIGKILL, power loss).
///
/// Conservative by construction: only real directories (never symlinks) that
/// this user owns are considered; a tree is removed only when it carries an
/// `.inka-owner` marker whose pid is no longer alive. Marker-less trees
/// predating the marker are reaped only once clearly old. Trees with a live pid
/// (i.e. a running artifact) are never touched.
fn sweep_stale_temp_trees() {
    sweep_stale_temp_trees_at(&std::env::temp_dir(), STALE_LEGACY_AGE);
}

fn sweep_stale_temp_trees_at(base: &Path, legacy_age: std::time::Duration) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for ent in entries.flatten() {
        let name = ent.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("inka-") {
            continue;
        }
        let path = ent.path();
        let Ok(md) = fs::symlink_metadata(&path) else {
            continue;
        };
        if md.file_type().is_symlink() || !md.is_dir() || !owned_by_us(&md) {
            continue;
        }
        match fs::read_to_string(path.join(OWNER_MARKER)) {
            Ok(text) => {
                let dead = text
                    .trim()
                    .parse::<u32>()
                    .map(|pid| !pid_alive(pid))
                    .unwrap_or(false); // unparseable marker: leave it
                if dead {
                    let _ = fs::remove_dir_all(&path);
                }
            }
            Err(_) => {
                if older_than(&md, legacy_age) {
                    let _ = fs::remove_dir_all(&path);
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn pid_alive(pid: u32) -> bool {
    pid == std::process::id() || Path::new("/proc").join(pid.to_string()).exists()
}

/// No reliable liveness probe off Linux, so never reap by pid there.
#[cfg(not(target_os = "linux"))]
fn pid_alive(_pid: u32) -> bool {
    true
}

fn older_than(md: &fs::Metadata, age: std::time::Duration) -> bool {
    match md.modified() {
        Ok(modified) => std::time::SystemTime::now()
            .duration_since(modified)
            .map(|since| since > age)
            .unwrap_or(false),
        Err(_) => false,
    }
}

#[cfg(unix)]
fn owned_by_us(md: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    match current_euid() {
        Some(euid) => md.uid() == euid,
        None => true, // unknown: fall back to filesystem permissions
    }
}

#[cfg(not(unix))]
fn owned_by_us(_md: &fs::Metadata) -> bool {
    true
}

#[cfg(unix)]
fn current_euid() -> Option<u32> {
    // `Uid:\t<real>\t<effective>\t<saved>\t<fs>`
    let status = fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().nth(1))
        .and_then(|euid| euid.parse().ok())
}

/// Materialize the embedded archive under a fresh, exclusive temp dir (mode
/// 0700), mirroring paths. Files are created with `create_new` (never following
/// a planted symlink), and the tree is removed when the guard drops.
fn extract_tree(files: &[(String, Vec<u8>)]) -> Result<TempTree, String> {
    let base = std::env::temp_dir();
    let mut root = None;
    for _ in 0..8 {
        let candidate = base.join(format!("inka-{}", random_hex(8)));
        match fs::create_dir(&candidate) {
            Ok(()) => {
                root = Some(candidate);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("cannot create {}: {e}", candidate.display())),
        }
    }
    let root = root.ok_or_else(|| "could not create a unique temp dir".to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&root, fs::Permissions::from_mode(0o700));
    }
    // Cover exits that skip Drop (the runtime may call exit() itself).
    tree_cleanup::arm(&root);
    for (path, data) in files {
        let target = root.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
        std::io::Write::write_all(&mut f, data)
            .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    }
    // Record the owning pid (after the payload, so an archive entry can never
    // collide with it) so a later run can reap this tree if we die without
    // cleanup (SIGKILL / power loss), where neither Drop nor atexit runs.
    let _ = fs::write(root.join(OWNER_MARKER), std::process::id().to_string());
    Ok(TempTree { root })
}

#[derive(Default)]
struct Manifest {
    min: Option<Version>,
    gt: Option<Version>,
    exact: Option<Version>,
    tested: Option<Version>,
    /// A present-but-unparseable `runtime=`/`tested-against=` value. A malformed
    /// constraint must not silently drop to "accept anything".
    malformed: Option<String>,
    module: String,
    /// Canonical permission lines (`permissions=…`, `allow-*=…`, `deny-*=…`)
    /// forwarded verbatim to the runtime.
    perms: String,
}

fn parse_manifest(bytes: &[u8]) -> Manifest {
    let mut m = Manifest {
        module: "main.js".into(),
        ..Default::default()
    };
    for raw in String::from_utf8_lossy(bytes).lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        let key = line[..eq].trim();
        let val = line[eq + 1..].trim();
        match key {
            "runtime" => {
                let rest = val.strip_prefix("inka_runtime").unwrap_or(val).trim_start();
                let (slot, text) = if let Some(x) = rest.strip_prefix(">=") {
                    (Slot::Min, x)
                } else if let Some(x) = rest.strip_prefix(">") {
                    (Slot::Gt, x)
                } else if let Some(x) = rest.strip_prefix("==") {
                    (Slot::Exact, x)
                } else {
                    (Slot::Exact, rest)
                };
                match parse_version(text) {
                    Some(v) => match slot {
                        Slot::Min => m.min = Some(v),
                        Slot::Gt => m.gt = Some(v),
                        Slot::Exact => m.exact = Some(v),
                    },
                    None => m.malformed = Some(format!("runtime={val}")),
                }
            }
            "tested-against" => match parse_version(val) {
                Some(v) => m.tested = Some(v),
                None => m.malformed = Some(format!("tested-against={val}")),
            },
            "module" => m.module = val.to_string(),
            "permissions" => {
                if !m.perms.is_empty() {
                    m.perms.push('\n');
                }
                m.perms.push_str(&format!("permissions={val}"));
            }
            _ if key.starts_with("allow-") || key.starts_with("deny-") => {
                if !m.perms.is_empty() {
                    m.perms.push('\n');
                }
                m.perms.push_str(&format!("{key}={val}"));
            }
            _ => {}
        }
    }
    m
}

enum Slot {
    Min,
    Gt,
    Exact,
}

/// Does `v` satisfy every constraint in the manifest?
fn constraint_allows(m: &Manifest, v: Version) -> bool {
    if let Some(e) = m.exact {
        if v != e {
            return false;
        }
    }
    if let Some(g) = m.gt {
        if v <= g {
            return false;
        }
    }
    if let Some(mn) = m.min {
        if v < mn {
            return false;
        }
    }
    if let Some(t) = m.tested {
        if v > t {
            return false;
        }
    }
    true
}

/// Confirm the loaded runtime's self-reported version matches the version its
/// filename claims. `inka_runtime_version()` returns `inka_runtime-<x.y.z>`.
fn check_reported_version(lib: &Path, reported: &str, expected: Version) -> Result<(), i32> {
    match reported
        .strip_prefix("inka_runtime-")
        .and_then(parse_version)
    {
        Some(v) if v == expected => Ok(()),
        Some(v) => {
            eprintln!(
                "[inka] runtime {} reports version {v}, but its filename says {expected}; \
                 refusing to load",
                lib.display()
            );
            Err(4)
        }
        None => {
            eprintln!(
                "[inka] runtime {} reports an unrecognized version '{reported}'; refusing to load",
                lib.display()
            );
            Err(4)
        }
    }
}

/// `$XDG_DATA_HOME` when absolute, else `$HOME/.local/share`.
fn xdg_data_root() -> Option<PathBuf> {
    if let Some(x) = env::var_os("XDG_DATA_HOME") {
        let p = PathBuf::from(x);
        if p.is_absolute() {
            return Some(p);
        }
    }
    env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share"))
}

fn runtime_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(h) = env::var_os("INKA_RUNTIME_HOME") {
        out.push(PathBuf::from(h));
    }
    if let Some(d) = xdg_data_root() {
        out.push(d.join("inka/runtime"));
    }
    out
}

#[cfg(unix)]
fn warn_if_world_writable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(md) = fs::metadata(dir) {
        if md.permissions().mode() & 0o002 != 0 {
            eprintln!(
                "[inka] warning: runtime dir {} is world-writable; a local user could \
                 replace the runtime",
                dir.display()
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_world_writable(_dir: &Path) {}

fn resolve_runtime(m: &Manifest, dirs: &[PathBuf]) -> Option<(Version, PathBuf)> {
    let mut best: Option<(Version, PathBuf)> = None;
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        warn_if_world_writable(dir);
        let Ok(rd) = fs::read_dir(dir) else { continue };
        for ent in rd.flatten() {
            // Skip symlinked entries: the version is the filename's claim, and a
            // symlink lets it point anywhere.
            if ent.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
                continue;
            }
            let name = ent.file_name().to_string_lossy().into_owned();
            let Some(stripped) = name.strip_prefix("libinka_runtime-") else {
                continue;
            };
            let Some(vstr) = stripped.strip_suffix(".so") else {
                continue;
            };
            let Some(v) = parse_version(vstr) else {
                continue;
            };
            if !constraint_allows(m, v) {
                continue;
            }
            if best.as_ref().is_none_or(|(bv, _)| v > *bv) {
                best = Some((v, ent.path()));
            }
        }
    }
    best
}

fn required_string(m: &Manifest) -> String {
    if let Some(e) = m.exact {
        format!("inka_runtime == {e}")
    } else if let Some(g) = m.gt {
        format!("inka_runtime > {g}")
    } else if let Some(x) = m.min {
        format!("inka_runtime >= {x}")
    } else {
        "inka_runtime any".into()
    }
}

fn load_and_run_dir(
    lib: &Path,
    expected: Version,
    dir: &str,
    entry: &str,
    args: &[String],
    perms: &str,
) -> i32 {
    let library = match load_runtime_library(lib) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[inka] failed to load {}: {e}", lib.display());
            return 1;
        }
    };

    type FnVersion = unsafe extern "C" fn() -> *const c_char;
    type FnRunDir = unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        *const c_char,
        c_int,
        *const *const c_char,
        *mut c_int,
        *mut *mut c_char,
        *const c_char,
    ) -> c_int;
    type FnFreeString = unsafe extern "C" fn(*mut c_char);

    unsafe {
        let ver: libloading::Symbol<FnVersion> = match library.get(b"inka_runtime_version") {
            Ok(s) => s,
            Err(_) => {
                eprintln!(
                    "[inka] runtime {} is missing inka_runtime_version; reinstall it",
                    lib.display()
                );
                return 4;
            }
        };
        let reported = CStr::from_ptr(ver()).to_string_lossy().into_owned();
        if let Err(code) = check_reported_version(lib, &reported, expected) {
            return code;
        }

        let dir_c = CString::new(dir).expect("nul in dir");
        let entry_c = CString::new(entry).expect("nul in entry");
        let perms_c = CString::new(perms).unwrap_or_else(|_| CString::new("").unwrap());
        let argv: Vec<CString> = args
            .iter()
            .map(|a| CString::new(a.as_str()).expect("nul byte in arg"))
            .collect();
        let mut argv_ptrs: Vec<*const c_char> = argv.iter().map(|c| c.as_ptr()).collect();
        argv_ptrs.push(std::ptr::null());

        let create: libloading::Symbol<unsafe extern "C" fn() -> *mut c_void> =
            match library.get(b"inka_runtime_create") {
                Ok(s) => s,
                Err(_) => {
                    eprintln!(
                        "[inka] runtime {} is missing inka_runtime_create; reinstall it",
                        lib.display()
                    );
                    return 4;
                }
            };
        let destroy: libloading::Symbol<unsafe extern "C" fn(*mut c_void)> =
            match library.get(b"inka_runtime_destroy") {
                Ok(s) => s,
                Err(_) => {
                    eprintln!(
                        "[inka] runtime {} is missing inka_runtime_destroy; reinstall it",
                        lib.display()
                    );
                    return 4;
                }
            };
        let run_dir: libloading::Symbol<FnRunDir> =
            match library.get(b"inka_runtime_run_module_dir") {
                Ok(s) => s,
                Err(_) => {
                    eprintln!(
                        "[inka] this artifact is multi-file but runtime {} does not support it \
                     (missing inka_runtime_run_module_dir); install a newer runtime",
                        lib.display()
                    );
                    return 4;
                }
            };

        let rt = create();
        let mut exit_code: c_int = 0;
        let mut err_msg: *mut c_char = std::ptr::null_mut();
        let rc = run_dir(
            rt,
            dir_c.as_ptr(),
            entry_c.as_ptr(),
            argv.len() as c_int,
            argv_ptrs.as_ptr(),
            &mut exit_code,
            &mut err_msg,
            perms_c.as_ptr(),
        );

        let free_string = library
            .get::<FnFreeString>(b"inka_runtime_free_string")
            .ok();

        if !err_msg.is_null() {
            eprintln!(
                "[inka] runtime error message: {}",
                CStr::from_ptr(err_msg).to_string_lossy()
            );
            if let Some(free) = free_string {
                free(err_msg);
            }
        }
        destroy(rt);

        if rc != 0 {
            eprintln!("[inka] runtime call failed (rc={rc})");
            return rc;
        }
        exit_code
    }
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    let me = fs::read_link("/proc/self/exe").expect("read /proc/self/exe");
    let bytes = fs::read(&me).expect("read own executable");

    let trailer = match parse_trailer(&bytes) {
        Ok(x) => x,
        Err(e) => {
            // Only the standalone launcher (no trailer) answers --version/-V.
            // An artifact has a valid trailer, so its args (including
            // `--version`) pass through to the program instead.
            if matches!(
                args.first().map(String::as_str),
                Some("--version") | Some("-V")
            ) {
                println!("inka-launcher {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            eprintln!("[inka] {e}");
            std::process::exit(2);
        }
    };

    // Reap trees from crashed runs before staging ours.
    sweep_stale_temp_trees();

    let manifest_bytes = match &trailer {
        Trailer::Archive { manifest, .. } => *manifest,
    };
    let m = parse_manifest(manifest_bytes);
    if let Some(bad) = &m.malformed {
        eprintln!("[inka] manifest has an unparseable version constraint: {bad}");
        std::process::exit(3);
    }
    let dirs = runtime_dirs();

    let Some((v, path)) = resolve_runtime(&m, &dirs) else {
        eprintln!("[inka] no compatible runtime found");
        eprintln!("[inka] required: {}", required_string(&m));
        if let Some(t) = m.tested {
            eprintln!("[inka] capped at tested-against {t}");
        }
        for d in &dirs {
            eprintln!("[inka]   searched: {}", d.display());
        }
        std::process::exit(3);
    };

    let code = match trailer {
        Trailer::Archive { files, .. } => {
            debug_log!("[inka] resolved inka_runtime {v} at {}", path.display());
            debug_log!("[inka] module '{}' archive {} files", m.module, files.len());
            let tree = match extract_tree(&files) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("[inka] failed to extract artifact tree: {e}");
                    std::process::exit(1);
                }
            };
            let code = load_and_run_dir(
                &path,
                v,
                &tree.path().to_string_lossy(),
                &m.module,
                &args,
                &m.perms,
            );
            // Remove the tree before exiting (process::exit skips destructors).
            drop(tree);
            code
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(path.len() as u64).to_le_bytes());
        v.extend_from_slice(&(data.len() as u64).to_le_bytes());
        v.extend_from_slice(path.as_bytes());
        v.extend_from_slice(data);
        v
    }

    #[test]
    fn archive_roundtrip() {
        let mut blob = entry("main.js", b"hi");
        blob.extend_from_slice(&entry("node_modules/x/index.js", b"x"));
        let files = parse_archive(&blob).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], ("main.js".to_string(), b"hi".to_vec()));
        assert_eq!(
            files[1],
            ("node_modules/x/index.js".to_string(), b"x".to_vec())
        );
    }

    #[test]
    fn archive_overflow_header_is_rejected() {
        // path_len = u64::MAX, data_len = 1: `path_len + data_len` must not wrap.
        let mut blob = Vec::new();
        blob.extend_from_slice(&u64::MAX.to_le_bytes());
        blob.extend_from_slice(&1u64.to_le_bytes());
        assert!(parse_archive(&blob).is_err());
    }

    #[test]
    fn archive_rejects_parent_dir() {
        assert!(parse_archive(&entry("../evil", b"x")).is_err());
        assert!(parse_archive(&entry("/abs", b"x")).is_err());
    }

    #[test]
    fn extract_tree_creates_and_cleans_up() {
        let files = vec![
            ("main.js".to_string(), b"console.log(1)".to_vec()),
            ("node_modules/x/index.js".to_string(), b"x".to_vec()),
        ];
        let root;
        {
            let tree = extract_tree(&files).unwrap();
            root = tree.path().to_path_buf();
            assert!(root.starts_with(std::env::temp_dir()));
            assert!(root.join("main.js").is_file());
            assert!(root.join("node_modules/x/index.js").is_file());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(&root).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o700, "temp tree must be 0700");
            }
        }
        assert!(!root.exists(), "tree must be removed on drop");
    }

    #[test]
    fn extract_tree_uses_distinct_roots() {
        let files = vec![("a".to_string(), b"a".to_vec())];
        let t1 = extract_tree(&files).unwrap();
        let t2 = extract_tree(&files).unwrap();
        assert_ne!(t1.path(), t2.path());
    }

    #[test]
    fn extracted_tree_carries_owner_marker() {
        let files = vec![("main.js".to_string(), b"x".to_vec())];
        let tree = extract_tree(&files).unwrap();
        let marker = fs::read_to_string(tree.path().join(OWNER_MARKER)).unwrap();
        assert_eq!(marker, std::process::id().to_string());
    }

    fn sweep_scratch() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "inka-sweep-{}-{}",
            std::process::id(),
            random_hex(4)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn sweep_reaps_dead_owner_and_keeps_live_and_symlinks() {
        let base = sweep_scratch();
        let dead = base.join("inka-dead");
        fs::create_dir_all(&dead).unwrap();
        fs::write(dead.join(OWNER_MARKER), "999999999").unwrap();
        let live = base.join("inka-live");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join(OWNER_MARKER), std::process::id().to_string()).unwrap();
        #[cfg(unix)]
        let link = {
            let l = base.join("inka-link");
            std::os::unix::fs::symlink(&dead, &l).unwrap();
            l
        };

        sweep_stale_temp_trees_at(&base, std::time::Duration::from_secs(3600));

        assert!(!dead.exists(), "dead-owner tree must be reaped");
        assert!(live.exists(), "live-owner tree must be kept");
        #[cfg(unix)]
        assert!(
            link.symlink_metadata().is_ok(),
            "symlink must be left alone"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn sweep_reaps_markerless_trees_only_when_old() {
        let base = sweep_scratch();
        let fresh = base.join("inka-fresh");
        fs::create_dir_all(&fresh).unwrap();
        let old = base.join("inka-old");
        fs::create_dir_all(&old).unwrap();

        // Fresh tree, one-day threshold: kept.
        sweep_stale_temp_trees_at(&base, std::time::Duration::from_secs(24 * 3600));
        assert!(fresh.exists(), "fresh marker-less tree must be kept");

        // Zero threshold makes the (just created) marker-less tree "old".
        sweep_stale_temp_trees_at(&base, std::time::Duration::ZERO);
        assert!(!old.exists(), "old marker-less tree must be reaped");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn manifest_operator_semantics() {
        let m = parse_manifest(b"runtime=inka_runtime>=0.266.2\n");
        assert_eq!(m.min, Some(Version(0, 266, 2)));
        assert!(m.gt.is_none() && m.exact.is_none() && m.malformed.is_none());

        let m = parse_manifest(b"runtime=inka_runtime>0.266.2\n");
        assert_eq!(m.gt, Some(Version(0, 266, 2)));
        assert!(m.min.is_none());

        let m = parse_manifest(b"runtime=inka_runtime==0.266.2\n");
        assert_eq!(m.exact, Some(Version(0, 266, 2)));

        let m = parse_manifest(b"runtime=0.266.2\n");
        assert_eq!(m.exact, Some(Version(0, 266, 2)));
    }

    #[test]
    fn malformed_version_is_detected() {
        assert!(parse_manifest(b"runtime=inka_runtime>=garbage\n")
            .malformed
            .is_some());
        assert!(parse_manifest(b"tested-against=nope\n").malformed.is_some());
    }

    #[test]
    fn strict_gt_and_constraints() {
        let m = parse_manifest(b"runtime=inka_runtime>0.266.2\n");
        assert!(
            !constraint_allows(&m, Version(0, 266, 2)),
            "> rejects equal"
        );
        assert!(constraint_allows(&m, Version(0, 266, 3)));
        assert!(!constraint_allows(&m, Version(0, 266, 1)));

        let m = parse_manifest(b"runtime=inka_runtime>=0.266.2\ntested-against=0.266.4\n");
        assert!(constraint_allows(&m, Version(0, 266, 2)));
        assert!(constraint_allows(&m, Version(0, 266, 4)));
        assert!(!constraint_allows(&m, Version(0, 266, 5)), "cap");

        let m = parse_manifest(b"runtime=inka_runtime==0.266.2\n");
        assert!(constraint_allows(&m, Version(0, 266, 2)));
        assert!(!constraint_allows(&m, Version(0, 266, 3)));
    }

    #[test]
    fn reported_version_must_match_filename() {
        let lib = Path::new("/x/libinka_runtime-0.266.2.so");
        assert!(check_reported_version(lib, "inka_runtime-0.266.2", Version(0, 266, 2)).is_ok());
        assert!(check_reported_version(lib, "inka_runtime-0.266.3", Version(0, 266, 2)).is_err());
        assert!(check_reported_version(lib, "garbage", Version(0, 266, 2)).is_err());
    }
}

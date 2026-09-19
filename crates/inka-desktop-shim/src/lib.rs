// Per-app desktop shim: the file the laufey backend loads as its "runtime".
//
// The laufey backend `dlopen`s `<app>.so` (derived from the executable name)
// and resolves the runtime C ABI (`laufey_runtime_init/start/shutdown`). This
// shim implements that ABI but contains no engine of its own: it extracts the
// app payload bundled next to/inside itself, locates the machine-shared
// `libinka_runtime-<tuple>.so`, and forwards the three entry points to it.
//
// That indirection is what lets many desktop apps share one heavy Deno
// runtime instead of each embedding a ~150MB copy.
//
// Payload resolution:
//   1. an `INKFOOT5` archive appended to this shim (the shipped layout), or
//   2. a sibling `app/` directory (dev/simple layout).
// Runtime resolution:
//   1. `$INKA_DESKTOP_RUNTIME` (exact path),
//   2. a co-located `libinka_runtime-*.so`,
//   3. `<data>/inka/runtime/libinka_runtime-<tuple>.so` from a sibling
//      `runtime-version` marker, else the newest there.
#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

type InitFn = unsafe extern "C" fn(*const c_void) -> c_int;
type StartFn = unsafe extern "C" fn() -> c_int;
type ShutdownFn = unsafe extern "C" fn();

struct Shared {
    init: InitFn,
    start: StartFn,
    shutdown: ShutdownFn,
}

static SHARED: OnceLock<Shared> = OnceLock::new();
static PAYLOAD: OnceLock<PathBuf> = OnceLock::new();
static APP_DYLIB: OnceLock<PathBuf> = OnceLock::new();

/// Path of this shim, via `dladdr`.
#[cfg(unix)]
fn self_path() -> Option<PathBuf> {
    #[repr(C)]
    struct DlInfo {
        dli_fname: *const c_char,
        dli_fbase: *mut c_void,
        dli_sname: *const c_char,
        dli_saddr: *mut c_void,
    }
    unsafe extern "C" {
        fn dladdr(addr: *const c_void, info: *mut DlInfo) -> c_int;
    }
    let mut info: DlInfo = unsafe { std::mem::zeroed() };
    if unsafe { dladdr(self_path as *const c_void, &mut info) } == 0 || info.dli_fname.is_null() {
        return None;
    }
    Some(PathBuf::from(
        unsafe { CStr::from_ptr(info.dli_fname) }
            .to_string_lossy()
            .into_owned(),
    ))
}

#[cfg(not(unix))]
fn self_path() -> Option<PathBuf> {
    None
}

fn data_runtime_dir() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("INKA_RUNTIME_HOME") {
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        let p = PathBuf::from(x);
        if p.is_absolute() {
            return Some(p.join("inka/runtime"));
        }
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/inka/runtime"))
}

fn cache_root() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_CACHE_HOME") {
        let p = PathBuf::from(x);
        if p.is_absolute() {
            return Some(p.join("inka/desktop"));
        }
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/inka/desktop"))
}

/// How many extracted payloads to keep cached (newest by mtime). Beyond this,
/// unlocked entries are pruned on launch so the cache can't grow unbounded.
const CACHE_KEEP: usize = 4;

/// Try a non-blocking `flock` on `file`. Returns true on success.
#[cfg(unix)]
fn flock_nb(file: &std::fs::File, exclusive: bool) -> bool {
    use std::os::unix::io::AsRawFd;
    let op = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    } | libc::LOCK_NB;
    unsafe { libc::flock(file.as_raw_fd(), op) == 0 }
}

#[cfg(not(unix))]
fn flock_nb(_file: &std::fs::File, _exclusive: bool) -> bool {
    false
}

/// Hold a shared lock on a payload directory for the life of the process, so a
/// concurrent launch's prune leaves an in-use payload alone.
fn lock_payload(dir: &Path) {
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(dir.join(".lock"))
    {
        if flock_nb(&file, false) {
            std::mem::forget(file);
        }
    }
}

/// Bound the payload cache: remove interrupted-extraction staging dirs and the
/// oldest unlocked extracted payloads beyond [`CACHE_KEEP`]. The currently
/// active payload is always skipped.
fn prune_cache(root: &Path, current: &Path) {
    let Ok(read) = std::fs::read_dir(root) else {
        return;
    };
    let mut extracted: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for ent in read.flatten() {
        let path = ent.path();
        if !path.is_dir() || path == current {
            continue;
        }
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if name.contains(".tmp") {
            let _ = std::fs::remove_dir_all(&path);
            continue;
        }
        let mtime = ent
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        extracted.push((mtime, path));
    }
    // Newest first; drop the tail beyond the keep count when unlocked.
    extracted.sort_by_key(|a| std::cmp::Reverse(a.0));
    for (_, path) in extracted.into_iter().skip(CACHE_KEEP) {
        let removable = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.join(".lock"))
        {
            Ok(file) => flock_nb(&file, true), // lock drops on scope end
            Err(_) => true,                    // no lock file => not in use
        };
        if removable {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Newest `libinka_runtime-*.so` in `dir`, or an exact tuple when given.
fn newest_runtime_in(dir: &Path, want: Option<&str>) -> Option<PathBuf> {
    if let Some(v) = want {
        let p = dir.join(format!("libinka_runtime-{v}.so"));
        if p.is_file() {
            return Some(p);
        }
    }
    let mut best: Option<(inka_format::Version, PathBuf)> = None;
    for ent in std::fs::read_dir(dir).ok()?.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        let Some(vs) = name
            .strip_prefix("libinka_runtime-")
            .and_then(|s| s.strip_suffix(".so"))
        else {
            continue;
        };
        let Some(v) = inka_format::parse_version(vs) else {
            continue;
        };
        if best.as_ref().is_none_or(|(bv, _)| v > *bv) {
            best = Some((v, ent.path()));
        }
    }
    best.map(|(_, p)| p)
}

/// Resolve the shared runtime given the shim's directory.
fn find_runtime(self_dir: &Path) -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("INKA_DESKTOP_RUNTIME") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    if let Some(p) = newest_runtime_in(self_dir, None) {
        return Ok(p);
    }
    let want = std::fs::read_to_string(self_dir.join("runtime-version"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let dir = data_runtime_dir().ok_or_else(|| "cannot determine the inka data dir".to_string())?;
    newest_runtime_in(&dir, want.as_deref()).ok_or_else(|| {
        format!(
            "no inka desktop runtime found in {} (run `inka update` or set INKA_DESKTOP_RUNTIME)",
            dir.display()
        )
    })
}

/// Extract an appended `INKFOOT5` archive (if present) and return the payload
/// root; otherwise fall back to a sibling `app/` directory.
fn resolve_payload(self_path: &Path) -> Result<PathBuf, String> {
    let self_dir = self_path
        .parent()
        .ok_or_else(|| "shim has no parent directory".to_string())?;
    let sibling = self_dir.join("app");
    if sibling.is_dir() {
        return Ok(sibling);
    }
    let bytes = std::fs::read(self_path)
        .map_err(|e| format!("cannot read {}: {e}", self_path.display()))?;
    let layout = match inka_format::read_layout(&bytes) {
        Ok(l) => l,
        Err(_) => return Err("no appended payload and no sibling app/ directory".to_string()),
    };
    let archive = &bytes[layout.archive_off..layout.archive_off + layout.archive_len];
    let files = inka_format::parse_archive(archive).map_err(|e| format!("bad payload: {e}"))?;

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(&bytes[layout.archive_off..layout.manifest_off]);
    let key: String = hasher.finalize()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    let cache = cache_root().ok_or_else(|| "cannot determine a cache dir".to_string())?;
    let root = cache.join(key);
    if root.join(".ok").is_file() {
        lock_payload(&root);
        prune_cache(&cache, &root);
        return Ok(root);
    }
    let staging = root.with_extension(format!("tmp{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    for (rel, data) in &files {
        inka_format::validate_rel_path(rel)?;
        let target = staging.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(&target, data)
            .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    }
    std::fs::write(staging.join(".ok"), b"ok").map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(&root);
    std::fs::rename(&staging, &root)
        .map_err(|e| format!("cannot activate payload {}: {e}", root.display()))?;
    lock_payload(&root);
    prune_cache(&cache, &root);
    Ok(root)
}

fn resolve_shared(runtime: &Path) -> Result<Shared, String> {
    let cpath = CString::new(runtime.to_string_lossy().as_bytes())
        .map_err(|_| "runtime path contains NUL".to_string())?;
    let handle = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        let err = unsafe { CStr::from_ptr(libc::dlerror()) }
            .to_string_lossy()
            .into_owned();
        return Err(format!("dlopen {}: {err}", runtime.display()));
    }
    // The library is intentionally never closed: it owns the V8 isolate for
    // the life of the process.
    unsafe fn sym(handle: *mut c_void, name: &str) -> Result<*mut c_void, String> {
        let c = CString::new(name).unwrap();
        let p = unsafe { libc::dlsym(handle, c.as_ptr()) };
        if p.is_null() {
            Err(format!("{name} not found in shared runtime"))
        } else {
            Ok(p)
        }
    }
    let init =
        unsafe { std::mem::transmute::<*mut c_void, InitFn>(sym(handle, "laufey_runtime_init")?) };
    let start = unsafe {
        std::mem::transmute::<*mut c_void, StartFn>(sym(handle, "laufey_runtime_start")?)
    };
    let shutdown = unsafe {
        std::mem::transmute::<*mut c_void, ShutdownFn>(sym(handle, "laufey_runtime_shutdown")?)
    };
    Ok(Shared {
        init,
        start,
        shutdown,
    })
}

/// Read the appended `INKFOOT5` manifest (footer + manifest region only).
fn read_appended_manifest(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len < 24 {
        return None;
    }
    f.seek(SeekFrom::Start(len - 24)).ok()?;
    let mut footer = [0u8; 24];
    f.read_exact(&mut footer).ok()?;
    if &footer[0..8] != b"INKFOOT5" {
        return None;
    }
    let mlen = u64::from_le_bytes(footer[16..24].try_into().ok()?) as usize;
    if mlen == 0 || (mlen as u64) > len - 24 {
        return None;
    }
    f.seek(SeekFrom::Start(len - 24 - mlen as u64)).ok()?;
    let mut buf = vec![0u8; mlen];
    f.read_exact(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn setup() -> Result<&'static Shared, String> {
    let self_path =
        self_path().ok_or_else(|| "cannot locate the shim itself (dladdr)".to_string())?;
    let _ = APP_DYLIB.set(self_path.clone());
    let app_dir = self_path.parent().unwrap_or(Path::new("."));
    let _ = PAYLOAD.set(resolve_payload(&self_path)?);
    // Surface manifest metadata to the shared runtime before it boots.
    if let Some(manifest) = read_appended_manifest(&self_path) {
        for line in manifest.lines() {
            let line = line.trim();
            for (key, var) in [
                ("app-name", "INKA_DESKTOP_APP_NAME"),
                ("module", "INKA_DESKTOP_ENTRY"),
                ("perms", "INKA_DESKTOP_PERMS"),
                ("version", "INKA_DESKTOP_APP_VERSION"),
                ("release-base", "INKA_DESKTOP_RELEASE_BASE"),
                ("error-reporting", "INKA_DESKTOP_ERROR_REPORTING"),
            ] {
                if let Some(v) = line.strip_prefix(key).and_then(|r| r.strip_prefix('=')) {
                    std::env::set_var(var, v);
                }
            }
        }
    }
    let runtime = find_runtime(app_dir)?;
    eprintln!(
        "[inka-desktop] runtime={} payload={}",
        runtime.display(),
        PAYLOAD
            .get()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );
    let _ = SHARED.set(resolve_shared(&runtime)?);
    Ok(SHARED.get().unwrap())
}

#[no_mangle]
pub unsafe extern "C" fn laufey_runtime_init(api: *const c_void) -> c_int {
    // Surface the resolved payload/dylib to the shared runtime before it boots.
    match setup() {
        Ok(shared) => {
            if let Some(p) = PAYLOAD.get() {
                std::env::set_var("INKA_DESKTOP_PAYLOAD", p);
            }
            if let Some(p) = APP_DYLIB.get() {
                std::env::set_var("INKA_DESKTOP_APP_DYLIB", p);
            }
            unsafe { (shared.init)(api) }
        }
        Err(e) => {
            eprintln!("[inka-desktop] shim init failed: {e}");
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn laufey_runtime_start() -> c_int {
    match SHARED.get() {
        Some(s) => unsafe { (s.start)() },
        None => -1,
    }
}

#[no_mangle]
pub extern "C" fn laufey_runtime_shutdown() {
    if let Some(s) = SHARED.get() {
        unsafe { (s.shutdown)() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_runtime_picks_max_tuple() {
        let dir = std::env::temp_dir().join(format!("inka-shim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for n in [
            "libinka_runtime-0.267.2.so",
            "libinka_runtime-0.267.10.so",
            "libinka_runtime-0.267.9-beta.1.so",
            "not-a-runtime.so",
        ] {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        let got = newest_runtime_in(&dir, None).unwrap();
        assert_eq!(got.file_name().unwrap(), "libinka_runtime-0.267.10.so");
        let exact = newest_runtime_in(&dir, Some("0.267.2")).unwrap();
        assert_eq!(exact.file_name().unwrap(), "libinka_runtime-0.267.2.so");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_appended_manifest() {
        let dir = std::env::temp_dir().join(format!("inka-shim-man-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("App.so");
        let files = vec![("main.js".to_string(), b"console.log(1)".to_vec())];
        let archive = inka_format::encode_archive(&files);
        let manifest = b"module=main.js\napp-name=Demo\n";
        let mut bytes = b"\x7fELFfake-shim".to_vec();
        bytes.extend_from_slice(&archive);
        bytes.extend_from_slice(manifest);
        bytes.extend_from_slice(&inka_format::encode_footer(
            archive.len() as u64,
            manifest.len() as u64,
        ));
        std::fs::write(&p, &bytes).unwrap();
        let m = read_appended_manifest(&p).expect("manifest");
        assert!(m.contains("app-name=Demo"), "{m}");
        assert!(m.contains("module=main.js"), "{m}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn payload_falls_back_to_sibling_app_dir() {
        let dir = std::env::temp_dir().join(format!("inka-shim-app-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("app")).unwrap();
        let fake = dir.join("MyApp.so");
        std::fs::write(&fake, b"not an archive").unwrap();
        assert_eq!(resolve_payload(&fake).unwrap(), dir.join("app"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_removes_stale_tmp_and_oldest_unlocked() {
        let root = std::env::temp_dir().join(format!("inka-shim-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let current = root.join("current");
        std::fs::create_dir_all(&current).unwrap();
        std::fs::write(current.join(".ok"), b"ok").unwrap();
        std::fs::create_dir_all(root.join("abc.tmp999")).unwrap();

        let base = std::time::SystemTime::now() - std::time::Duration::from_secs(10_000);
        let mut dirs = Vec::new();
        for i in 0..6u64 {
            let d = root.join(format!("payload{i}"));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(".ok"), b"ok").unwrap();
            std::fs::File::open(&d)
                .unwrap()
                .set_modified(base + std::time::Duration::from_secs(i))
                .unwrap();
            dirs.push(d);
        }
        // Lock the oldest payload; it must survive the prune.
        let locked = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(dirs[0].join(".lock"))
            .unwrap();
        assert!(flock_nb(&locked, true));

        prune_cache(&root, &current);

        assert!(
            !root.join("abc.tmp999").exists(),
            "stale staging dir pruned"
        );
        assert!(current.exists(), "current skipped");
        assert!(dirs[0].exists(), "locked payload kept");
        let remaining = (0..6)
            .filter(|i| root.join(format!("payload{i}")).exists())
            .count();
        assert!(
            remaining >= CACHE_KEEP,
            "must keep at least CACHE_KEEP payloads, kept {remaining}"
        );
        assert!(
            remaining < 6,
            "prune should remove something, kept {remaining}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

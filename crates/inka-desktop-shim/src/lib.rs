// Per-app desktop shim: the file the laufey backend loads as its "runtime".
//
// The laufey backend loads `<app>.<dylib ext>` (derived from the executable
// name) and resolves the runtime C ABI (`laufey_runtime_init/start/shutdown`).
// This shim implements that ABI but contains no engine of its own: it reads the
// app payload from the `inka` binary section embedded in itself (via `libsui`,
// the same mechanism `libdenort` uses), locates the machine-shared
// `libinka_runtime-<tuple>.<ext>`, and forwards the three entry points to it.
//
// That indirection is what lets many desktop apps share one heavy Deno
// runtime instead of each embedding a ~150MB copy.
//
// Payload resolution:
//   1. the embedded `inka` section (the shipped layout), or
//   2. a sibling `app/` directory (dev/simple layout).
// Runtime resolution:
//   1. `$INKA_DESKTOP_RUNTIME` (exact path),
//   2. a co-located `libinka_runtime-*.<ext>`,
//   3. `<data>/inka/runtime/libinka_runtime-<tuple>.<ext>` from a sibling
//      `runtime-version` marker, else the newest there.
#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_int, c_void};
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

fn data_runtime_dir() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("INKA_RUNTIME_HOME") {
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    Some(inka_format::platform::data_dir().join("inka/runtime"))
}

fn cache_root() -> Option<PathBuf> {
    inka_format::platform::cache_root().map(|c| c.join("inka/desktop"))
}

/// How many extracted payloads to keep cached (newest by mtime). Beyond this,
/// unlocked entries are pruned on launch so the cache can't grow unbounded.
const CACHE_KEEP: usize = 4;

/// Try a non-blocking exclusive lock on `path` (creating the lock file). Holds
/// the lock only if acquired; the caller keeps the returned handle alive.
fn try_lock_file(path: &Path) -> Option<fslock::LockFile> {
    let mut lock = fslock::LockFile::open(path).ok()?;
    match lock.try_lock() {
        Ok(true) => Some(lock),
        _ => None,
    }
}

/// Hold a lock on a payload directory for the life of the process, so a
/// concurrent launch's prune leaves an in-use payload alone.
fn lock_payload(dir: &Path) {
    if let Some(lock) = try_lock_file(&dir.join(".lock")) {
        std::mem::forget(lock);
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
        // Take the lock, release it, then remove: Windows cannot delete a file
        // with an open handle, and the race is acceptable for a best-effort
        // cache prune.
        let removable = match try_lock_file(&path.join(".lock")) {
            Some(lock) => {
                drop(lock);
                true
            }
            None => !path.join(".lock").exists(),
        };
        if removable {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Newest `libinka_runtime-*.<ext>` in `dir`, or an exact tuple when given.
fn newest_runtime_in(dir: &Path, want: Option<&str>) -> Option<PathBuf> {
    if let Some(v) = want {
        let p = dir.join(inka_format::platform::runtime_lib_name(v));
        if p.is_file() {
            return Some(p);
        }
    }
    let mut best: Option<(inka_format::Version, PathBuf)> = None;
    for ent in std::fs::read_dir(dir).ok()?.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        let Some(vs) = name
            .strip_prefix(inka_format::platform::RUNTIME_LIB_PREFIX)
            .and_then(|s| s.strip_suffix(inka_format::platform::runtime_lib_suffix()))
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

/// Extract the embedded `inka` section payload (if present) and return its
/// on-disk root; otherwise fall back to a sibling `app/` directory (dev).
fn resolve_payload(embedded: Option<&[u8]>, self_dir: &Path) -> Result<PathBuf, String> {
    let sibling = self_dir.join("app");
    if sibling.is_dir() {
        return Ok(sibling);
    }
    let payload = embedded
        .ok_or_else(|| "no embedded payload section and no sibling app/ directory".to_string())?;
    let section = inka_format::read_section_payload(payload)
        .map_err(|e| format!("bad embedded payload: {e}"))?;
    let files =
        inka_format::parse_archive(section.archive).map_err(|e| format!("bad payload: {e}"))?;

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(section.archive);
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

/// Load the shared runtime with the cross-platform loader Deno's `deno_napi`
/// uses (`libloading`). On unix we keep `RTLD_GLOBAL` so native N-API addons
/// `dlopen`ed later can resolve the runtime's symbols; on Windows the loader's
/// default search is already process-global.
#[cfg(unix)]
fn load_runtime(path: &Path) -> Result<libloading::Library, libloading::Error> {
    use libloading::os::unix::{Library as UnixLibrary, RTLD_GLOBAL, RTLD_LAZY};
    unsafe { UnixLibrary::open(Some(path), RTLD_LAZY | RTLD_GLOBAL).map(libloading::Library::from) }
}

#[cfg(not(unix))]
fn load_runtime(path: &Path) -> Result<libloading::Library, libloading::Error> {
    unsafe { libloading::Library::new(path) }
}

fn resolve_shared(runtime: &Path) -> Result<Shared, String> {
    let library =
        load_runtime(runtime).map_err(|e| format!("cannot load {}: {e}", runtime.display()))?;

    unsafe fn sym<T: Copy>(library: &libloading::Library, name: &str) -> Result<T, String> {
        // SAFETY: the symbol type is asserted by the caller to match the C ABI
        // exported by the shared runtime.
        let symbol = library
            .get::<T>(name.as_bytes())
            .map_err(|e| format!("{name} not found in shared runtime: {e}"))?;
        Ok(*symbol)
    }

    let init = unsafe { sym::<InitFn>(&library, "laufey_runtime_init")? };
    let start = unsafe { sym::<StartFn>(&library, "laufey_runtime_start")? };
    let shutdown = unsafe { sym::<ShutdownFn>(&library, "laufey_runtime_shutdown")? };

    // The library is intentionally never closed: it owns the V8 isolate for the
    // life of the process.
    std::mem::forget(library);
    Ok(Shared {
        init,
        start,
        shutdown,
    })
}

fn setup() -> Result<&'static Shared, String> {
    // A GUI-subsystem backend may have no valid stdio handles; repair them
    // before anything writes to stderr (Windows only; a no-op elsewhere).
    inka_format::platform::ensure_stdio_open();
    // The backend executable and this shim are co-located, and the shim is
    // named `<App>.<ext>` after the backend. `current_exe()` therefore gives
    // both the app directory and the shim path without any dladdr/self-path
    // probing.
    let exe = std::env::current_exe()
        .map_err(|e| format!("cannot locate the desktop executable: {e}"))?;
    let app_dir = exe.parent().unwrap_or(Path::new("."));
    let _ = APP_DYLIB.set(exe.with_extension(inka_format::platform::dylib_ext()));

    let embedded = libsui::find_section_in_current_image(inka_format::SECTION_NAME)
        .map_err(|e| format!("cannot read the embedded payload section: {e}"))?;
    let _ = PAYLOAD.set(resolve_payload(embedded, app_dir)?);

    // Surface manifest metadata to the shared runtime before it boots.
    if let Some(payload) = embedded {
        let section = inka_format::read_section_payload(payload)
            .map_err(|e| format!("bad embedded payload: {e}"))?;
        for line in String::from_utf8_lossy(section.manifest).lines() {
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
    fn payload_falls_back_to_sibling_app_dir() {
        let dir = std::env::temp_dir().join(format!("inka-shim-app-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("app")).unwrap();
        assert_eq!(resolve_payload(None, &dir).unwrap(), dir.join("app"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn payload_missing_without_app_or_section_is_an_error() {
        let dir = std::env::temp_dir().join(format!("inka-shim-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(resolve_payload(None, &dir).is_err());
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
        let locked = try_lock_file(&dirs[0].join(".lock")).expect("take payload lock");

        prune_cache(&root, &current);

        assert!(
            !root.join("abc.tmp999").exists(),
            "stale staging dir pruned"
        );
        assert!(current.exists(), "current skipped");
        assert!(dirs[0].exists(), "locked payload kept");
        drop(locked);
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

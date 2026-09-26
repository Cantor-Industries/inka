// Shared, per-machine CEF runtime for `inka desktop --backend cef`.
//
// The laufey CEF backend links `libcef.so`/`libcef.dll` (RPATH=.:$ORIGIN) and
// reads its Chromium resources from beside the executable, so those files must
// be reachable from every app dir. Instead of copying ~360 MB per app, inka
// keeps one versioned copy under the data dir and links/copies it in:
//
//   ~/.local/share/cef/<laufey-version>/<target>/
//
// The launcher itself is never shared: laufey derives the runtime library name
// from its own executable path (`<exe-stem>.so`), so it stays a real per-app
// file.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use crate::desktop::LAUFEY_VERSION;
use crate::platform;

/// Marker recording the version/target a shared dir was populated from.
const INSTALLED_MARKER: &str = ".installed";

/// Where the shared CEF runtime lives: `INKA_CEF_HOME`, else
/// `$XDG_DATA_HOME/cef/<version>/<target>` (`~/.local/share/cef/...`).
///
/// It sits at the data root rather than under `inka/` so other CEF-based apps
/// can share the same versioned runtime.
pub(crate) fn shared_cef_dir() -> PathBuf {
    if let Some(h) = env::var_os("INKA_CEF_HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    crate::data_root_now()
        .join("cef")
        .join(LAUFEY_VERSION)
        .join(platform::laufey_target())
}

/// CEF runtime entries are everything in the laufey backend directory except
/// the launcher itself, dot-markers/caches, and CEF runtime state.
fn is_cef_runtime_entry(name: &OsStr, backend_exe: &OsStr) -> bool {
    if name == backend_exe {
        return false;
    }
    match name.to_str() {
        Some(s) => !s.starts_with('.') && s != "extensions",
        None => true,
    }
}

/// Copy one filesystem entry, recreating symlinks and preserving modes.
fn copy_entry(from: &Path, to: &Path) -> Result<(), String> {
    let md =
        fs::symlink_metadata(from).map_err(|e| format!("cannot stat {}: {e}", from.display()))?;
    let ft = md.file_type();
    if ft.is_dir() {
        copy_dir_all(from, to)
    } else if ft.is_symlink() {
        let target =
            fs::read_link(from).map_err(|e| format!("cannot read link {}: {e}", from.display()))?;
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, to)
                .map_err(|e| format!("cannot link {}: {e}", to.display()))?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = target;
            Err(format!(
                "symlink copies are unsupported on this platform: {}",
                from.display()
            ))
        }
    } else {
        fs::copy(from, to).map_err(|e| format!("copy {}: {e}", from.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = fs::metadata(to) {
                let mut perms = meta.permissions();
                perms.set_mode(perms.mode() | 0o200);
                let _ = fs::set_permissions(to, perms);
            }
        }
        Ok(())
    }
}

/// Recursively copy `src` into `dst`, recreating symlinks and preserving
/// permission bits, and forcing copied files owner-writable (backend caches can
/// be read-only). Adapted from Deno's `cli/tools/compile.rs`.
pub(crate) fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| format!("cannot create {}: {e}", dst.display()))?;
    for ent in fs::read_dir(src)
        .map_err(|e| format!("cannot read {}: {e}", src.display()))?
        .flatten()
    {
        copy_entry(&ent.path(), &dst.join(ent.file_name()))?;
    }
    Ok(())
}

/// Copy the CEF runtime entries from a laufey backend dir into `dest`
/// (top-level filtering only; shared subdirectories like `locales/` are copied
/// wholesale).
fn copy_cef_runtime(src: &Path, backend_exe: &OsStr, dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    for ent in fs::read_dir(src)
        .map_err(|e| format!("cannot read {}: {e}", src.display()))?
        .flatten()
    {
        if !is_cef_runtime_entry(&ent.file_name(), backend_exe) {
            continue;
        }
        copy_entry(&ent.path(), &dest.join(ent.file_name()))?;
    }
    Ok(())
}

/// Whether `shared` already holds the CEF runtime for this laufey version/target.
fn shared_installed(shared: &Path) -> bool {
    let expected = format!("v{LAUFEY_VERSION} {}", platform::laufey_target());
    match fs::read_to_string(shared.join(INSTALLED_MARKER)) {
        Ok(marker) if marker.trim() == expected => shared.join(platform::cef_lib_name()).is_file(),
        _ => false,
    }
}

/// Populate `shared` from the verified laufey backend dir at `src`, idempotently.
pub(crate) fn ensure_shared_cef_at(
    src: &Path,
    backend_exe: &Path,
    shared: &Path,
) -> Result<(), String> {
    if shared_installed(shared) {
        return Ok(());
    }
    let backend_exe = backend_exe
        .file_name()
        .ok_or_else(|| format!("backend has no file name: {}", backend_exe.display()))?;

    let file_name = shared
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cef".to_string());
    let tmp = shared.with_file_name(format!("{file_name}.tmp-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    if let Some(parent) = shared.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }

    copy_cef_runtime(src, backend_exe, &tmp)?;
    let cef_lib = platform::cef_lib_name();
    if !tmp.join(cef_lib).is_file() {
        let _ = fs::remove_dir_all(&tmp);
        return Err(format!("no {cef_lib} to share from {}", src.display()));
    }
    // Replace any stale/partial install atomically.
    let _ = fs::remove_dir_all(shared);
    fs::rename(&tmp, shared).map_err(|e| {
        let _ = fs::remove_dir_all(&tmp);
        format!("cannot publish {}: {e}", shared.display())
    })?;
    fs::write(
        shared.join(INSTALLED_MARKER),
        format!("v{LAUFEY_VERSION} {}\n", platform::laufey_target()),
    )
    .map_err(|e| {
        format!(
            "cannot write {}: {e}",
            shared.join(INSTALLED_MARKER).display()
        )
    })?;
    Ok(())
}

/// Populate (once) and return the shared CEF runtime dir.
pub(crate) fn ensure_shared_cef(src: &Path, backend_exe: &Path) -> Result<PathBuf, String> {
    let shared = shared_cef_dir();
    ensure_shared_cef_at(src, backend_exe, &shared)?;
    Ok(shared)
}

/// Remove a file, symlink, or directory at `path` (never following a symlink).
fn remove_any(path: &Path) {
    if let Ok(md) = fs::symlink_metadata(path) {
        if md.is_dir() {
            let _ = fs::remove_dir_all(path);
        } else {
            let _ = fs::remove_file(path);
        }
    }
}

/// Symlink the shared CEF runtime into `out`, so laufey's `.:$ORIGIN` RPATH and
/// CEF's resource lookup find it beside the launcher. `chrome-sandbox` is
/// copied for real: Chromium security-checks that helper and may reject a link.
///
/// Returns `Err` (with any partially-created links left in place) so the caller
/// can fall back to a self-contained copy.
pub(crate) fn link_into_app(shared: &Path, out: &Path) -> Result<(), String> {
    for ent in fs::read_dir(shared)
        .map_err(|e| format!("cannot read {}: {e}", shared.display()))?
        .flatten()
    {
        let name = ent.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if name_str == INSTALLED_MARKER || name_str.starts_with('.') || name_str == "extensions" {
            continue;
        }
        let from = shared.join(&name);
        let to = out.join(&name);
        remove_any(&to);
        if name_str == "chrome-sandbox" {
            copy_entry(&from, &to)?;
            continue;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&from, &to)
            .map_err(|e| format!("cannot link {} -> {}: {e}", to.display(), from.display()))?;
        #[cfg(not(unix))]
        return Err("sharing the CEF runtime requires symlinks".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let d = env::temp_dir().join(format!(
            "inka-cef-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn fake_backend(dir: &Path) {
        fs::create_dir_all(dir.join("locales")).unwrap();
        fs::write(dir.join("laufey"), b"exe").unwrap();
        fs::write(dir.join(platform::cef_lib_name()), b"cef").unwrap();
        fs::write(dir.join("chrome-sandbox"), b"sandbox").unwrap();
        fs::write(dir.join("locales/en-US.pak"), b"pak").unwrap();
        fs::write(dir.join(".downloaded"), b"v\n").unwrap();
        fs::write(dir.join(".laufey.cache"), b"cache").unwrap();
        fs::create_dir_all(dir.join(".laufey")).unwrap();
        fs::write(dir.join(".laufey/x"), b"x").unwrap();
    }

    #[test]
    fn copy_dir_all_preserves_tree_symlinks_and_modes() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let base = scratch("copy");
            let src = base.join("src");
            let dst = base.join("dst");
            fs::create_dir_all(src.join("locales")).unwrap();
            fs::write(src.join("libcef.so"), b"cef").unwrap();
            fs::write(src.join("locales/en-US.pak"), b"pak").unwrap();
            let run = src.join("run.sh");
            fs::write(&run, b"#!/bin/sh\n").unwrap();
            fs::set_permissions(&run, fs::Permissions::from_mode(0o555)).unwrap();
            std::os::unix::fs::symlink("libcef.so", src.join("libcef.so.1")).unwrap();

            copy_dir_all(&src, &dst).unwrap();

            assert_eq!(fs::read(dst.join("libcef.so")).unwrap(), b"cef");
            assert_eq!(fs::read(dst.join("locales/en-US.pak")).unwrap(), b"pak");
            let link = fs::symlink_metadata(dst.join("libcef.so.1")).unwrap();
            assert!(link.file_type().is_symlink(), "symlink must be recreated");
            assert_eq!(
                fs::read_link(dst.join("libcef.so.1")).unwrap(),
                PathBuf::from("libcef.so")
            );
            let mode = fs::metadata(dst.join("run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o200, 0o200, "copied file must be writable");
            assert_eq!(mode & 0o100, 0o100, "exec bit must be preserved");
            let _ = fs::remove_dir_all(&base);
        }
    }

    #[test]
    fn ensure_shared_cef_populates_and_is_idempotent() {
        let base = scratch("ensure");
        let backend = base.join("backend");
        let shared = base.join("shared/cef");
        fake_backend(&backend);

        ensure_shared_cef_at(&backend, &backend.join("laufey"), &shared).unwrap();

        assert!(shared.join(platform::cef_lib_name()).is_file());
        assert!(shared.join("locales/en-US.pak").is_file());
        assert!(shared.join("chrome-sandbox").is_file());
        assert!(!shared.join("laufey").exists(), "launcher is not shared");
        assert!(!shared.join(".downloaded").exists());
        assert!(!shared.join(".laufey").exists());
        assert!(!shared.join(".laufey.cache").exists());
        assert!(shared.join(INSTALLED_MARKER).is_file());

        // A second call reuses the install (and does not fail on the marker).
        ensure_shared_cef_at(&backend, &backend.join("laufey"), &shared).unwrap();
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn link_into_app_symlinks_runtime_and_copies_sandbox() {
        let base = scratch("link");
        let backend = base.join("backend");
        let shared = base.join("shared/cef");
        let out = base.join("app");
        fake_backend(&backend);
        ensure_shared_cef_at(&backend, &backend.join("laufey"), &shared).unwrap();
        fs::create_dir_all(&out).unwrap();

        link_into_app(&shared, &out).unwrap();

        let link = fs::symlink_metadata(out.join("libcef.so")).unwrap();
        assert!(link.file_type().is_symlink(), "libcef.so must be a symlink");
        assert_eq!(
            fs::read_link(out.join("libcef.so")).unwrap(),
            shared.join("libcef.so")
        );
        assert!(fs::read(out.join("locales/en-US.pak")).unwrap() == b"pak");
        assert!(
            !fs::symlink_metadata(out.join("chrome-sandbox"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "chrome-sandbox must be a real copy"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn shared_cef_dir_is_versioned_under_data_root() {
        // `INKA_CEF_HOME` is process-global, so only assert the default shape
        // when it is unset (the test harness does not set it).
        if env::var_os("INKA_CEF_HOME").is_none() {
            let d = shared_cef_dir();
            assert!(
                d.ends_with(
                    PathBuf::from("cef")
                        .join(LAUFEY_VERSION)
                        .join(platform::laufey_target())
                ),
                "got {}",
                d.display()
            );
            assert!(
                !d.to_string_lossy().contains("inka/cef"),
                "the shared CEF dir must not be namespaced under inka: {}",
                d.display()
            );
        }
    }
}

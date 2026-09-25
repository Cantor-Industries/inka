//! Host-platform facts shared by the CLI, the launcher, and the desktop shim:
//! the target triple, the on-disk names of inka's binaries, and the per-user
//! data/cache directories.
//!
//! Only `x86_64-unknown-linux-gnu` and `x86_64-pc-windows-msvc` are supported;
//! an unsupported target fails the build rather than silently mis-naming files.

use std::path::PathBuf;

#[cfg(unix)]
use std::ffi::OsStr;

#[cfg(windows)]
pub mod windows;

/// Ensure stdin/stdout/stderr have valid handles. On Windows a GUI-subsystem
/// process may have none, and constructing a `std::fs::File` from a null stdio
/// handle panics. Vendored from Deno's `cli/util/windows.rs`. A no-op elsewhere.
#[cfg(windows)]
pub use windows::ensure_stdio_open;
#[cfg(not(windows))]
pub fn ensure_stdio_open() {}

// ---- target ----------------------------------------------------------------

#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
pub const TARGET: &str = "x86_64-unknown-linux-gnu";

#[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]
pub const TARGET: &str = "x86_64-pc-windows-msvc";

#[cfg(not(any(
    all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"),
    all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"),
)))]
compile_error!("inka supports only x86_64-unknown-linux-gnu and x86_64-pc-windows-msvc");

/// Target triple used to select laufey backend archives.
pub const fn laufey_target() -> &'static str {
    TARGET
}

/// The executable suffix (empty except on Windows).
pub const fn exe_suffix() -> &'static str {
    if cfg!(target_os = "windows") {
        ".exe"
    } else {
        ""
    }
}

// ---- names -----------------------------------------------------------------

/// Prefix of a shared-runtime library file name.
pub const RUNTIME_LIB_PREFIX: &str = "libinka_runtime-";

/// Extension of the shared-runtime library (`.so` on unix, `.dll` on Windows).
pub const fn runtime_lib_suffix() -> &'static str {
    if cfg!(target_os = "windows") {
        ".dll"
    } else {
        ".so"
    }
}

/// Shared-library extension without the leading dot (`so`/`dll`), for
/// `Path::with_extension`.
pub const fn dylib_ext() -> &'static str {
    if cfg!(target_os = "windows") {
        "dll"
    } else {
        "so"
    }
}

/// The shared-runtime library file name for a version tuple.
pub fn runtime_lib_name(version: &str) -> String {
    format!("{RUNTIME_LIB_PREFIX}{version}{}", runtime_lib_suffix())
}

/// The per-app desktop shim library file name.
pub const fn shim_lib_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "libinka_desktop_shim.dll"
    } else {
        "libinka_desktop_shim.so"
    }
}

/// The launcher executable name.
pub const fn launcher_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "inka-launcher.exe"
    } else {
        "inka-launcher"
    }
}

/// The shared CEF runtime library file name.
pub const fn cef_lib_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "libcef.dll"
    } else {
        "libcef.so"
    }
}

/// Mark a path executable: `chmod 0o755` on unix; a no-op on Windows.
#[cfg(unix)]
pub fn set_exec(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

/// Mark a path executable: `chmod 0o755` on unix; a no-op on Windows.
#[cfg(not(unix))]
pub fn set_exec(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

// ---- per-user directories --------------------------------------------------

/// Per-user data root. Unix: `$XDG_DATA_HOME` (non-empty, absolute), else
/// `$HOME/.local/share`, else `.`. Windows: `%LOCALAPPDATA%`, else `.`.
#[cfg(unix)]
pub fn data_root(home: Option<&OsStr>, xdg: Option<&OsStr>) -> PathBuf {
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

/// `data_dir` with an injectable environment (unix).
#[cfg(unix)]
pub fn data_dir() -> PathBuf {
    data_root(
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("XDG_DATA_HOME").as_deref(),
    )
}

/// `%LOCALAPPDATA%`, else `.`.
#[cfg(windows)]
pub fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Base per-user cache directory, when one can be determined. Unix:
/// `$XDG_CACHE_HOME` (non-empty, absolute), else `$HOME/.cache`. Windows:
/// `%LOCALAPPDATA%`.
pub fn cache_root() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        if let Some(x) = std::env::var_os("XDG_CACHE_HOME") {
            if !x.is_empty() {
                let p = PathBuf::from(x);
                if p.is_absolute() {
                    return Some(p);
                }
            }
        }
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(|h| PathBuf::from(h).join(".cache"))
    }
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }
}

/// Effective Deno cache dir (unix): `$DENO_DIR` (non-empty), else
/// `$HOME/.cache/deno` (`$HOME` defaults to `.`).
#[cfg(unix)]
pub fn deno_dir_root(env_deno: Option<&OsStr>, home: Option<&OsStr>) -> PathBuf {
    if let Some(d) = env_deno {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    let home = home
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| OsStr::new("."));
    PathBuf::from(home).join(".cache/deno")
}

/// Effective Deno cache dir: `$DENO_DIR` (non-empty), else `<cache_root>/deno`
/// (or `./.cache/deno` on unix when no home is known).
pub fn deno_dir() -> PathBuf {
    #[cfg(unix)]
    {
        deno_dir_root(
            std::env::var_os("DENO_DIR").as_deref(),
            std::env::var_os("HOME").as_deref(),
        )
    }
    #[cfg(windows)]
    {
        if let Some(d) = std::env::var_os("DENO_DIR") {
            if !d.is_empty() {
                return PathBuf::from(d);
            }
        }
        data_dir().join("deno")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::ffi::OsStr;

    #[cfg(unix)]
    fn os(s: &str) -> &OsStr {
        OsStr::new(s)
    }

    #[test]
    fn runtime_lib_name_matches_platform() {
        assert_eq!(
            runtime_lib_name("1.2.3"),
            format!("libinka_runtime-1.2.3{}", runtime_lib_suffix())
        );
    }

    #[test]
    #[cfg(unix)]
    fn data_root_prefers_absolute_xdg() {
        assert_eq!(
            data_root(Some(os("/home/u")), Some(os("/xdg"))),
            PathBuf::from("/xdg")
        );
    }

    #[test]
    #[cfg(unix)]
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
    #[cfg(unix)]
    fn data_root_falls_back_to_home_then_dot() {
        assert_eq!(
            data_root(Some(os("/home/u")), None),
            PathBuf::from("/home/u/.local/share")
        );
        assert_eq!(data_root(None, None), PathBuf::from("."));
        assert_eq!(data_root(Some(os("")), None), PathBuf::from("."));
    }

    #[test]
    #[cfg(unix)]
    fn xdg_runtime_is_under_inka() {
        let root = data_root(Some(os("/home/u")), None);
        assert_eq!(
            root.join("inka/runtime"),
            PathBuf::from("/home/u/.local/share/inka/runtime")
        );
    }

    #[test]
    #[cfg(unix)]
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
}

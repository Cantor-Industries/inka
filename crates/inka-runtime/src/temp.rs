//! Random, exclusive temp trees with cleanup on drop and on `exit()`.
//!
//! The runtime stages a single-file entry as a one-file tree. The tree must be
//! unpredictable (no planted-symlink races), exclusive, and removed even when
//! the embedded runtime calls `exit()` (e.g. `Deno.exit`), which skips Rust
//! destructors. This mirrors the launcher's `TempTree`.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Hex string from `/dev/urandom` (fallback: pid + time), for unpredictable
/// temp names. No new dependency; the runtime already assumes Linux.
fn random_hex(bytes: usize) -> String {
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

#[cfg(unix)]
mod cleanup {
    use std::path::Path;
    use std::sync::{Once, OnceLock};

    static ROOT: OnceLock<String> = OnceLock::new();
    static ARM: Once = Once::new();

    extern "C" fn run() {
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
            atexit(run);
        });
    }
}

#[cfg(not(unix))]
mod cleanup {
    use std::path::Path;
    pub(super) fn arm(_root: &Path) {}
}

/// A freshly created, exclusive temp tree, removed when dropped.
pub(crate) struct TempTree {
    root: PathBuf,
}

impl TempTree {
    /// Create a new exclusive tree under the system temp dir (mode 0700),
    /// retrying on name collisions. The tree is armed for `atexit` cleanup so an
    /// `exit()` from inside the runtime cannot leak it.
    pub(crate) fn create() -> Result<Self, String> {
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
        cleanup::arm(&root);
        Ok(Self { root })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

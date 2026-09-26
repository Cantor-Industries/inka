//! Hardened extraction of downloaded archives (toolchain + laufey backends).
//!
//! Vendored and adapted from Deno's `cli/tools/desktop.rs`
//! (`extract_laufey_archive`) and `cli/util/extract.rs` (MIT, Copyright (c) the
//! Deno authors): entries are confined to the destination, archive paths with
//! `..`/absolute components are rejected, zip symlink entries are refused, and
//! unix permission bits are normalized (setuid/setgid dropped, exec preserved).
//!
//! The archives inka extracts are checksum-verified against pinned digests
//! before this runs; these checks are defense-in-depth.

use std::fs;
use std::io;
use std::path::{Component, Path};

/// Extract `archive_path` into `dest`, choosing the format from `archive_name`
/// (`.zip` or `.tar.gz`/`.tgz`).
pub(crate) fn extract(archive_name: &str, archive_path: &Path, dest: &Path) -> Result<(), String> {
    let lower = archive_name.to_ascii_lowercase();
    let data = fs::read(archive_path)
        .map_err(|e| format!("cannot read {}: {e}", archive_path.display()))?;
    fs::create_dir_all(dest).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    if lower.ends_with(".zip") {
        extract_zip(&data, dest)
    } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        extract_tar_gz(&data, dest)
    } else {
        Err(format!("unsupported archive format: {archive_name}"))
    }
}

/// Reject an entry path with a root/prefix or `..` component, independent of
/// the platform's notion of "absolute" (`enclosed_name` is target-relative).
fn reject_unsafe_path(rel: &Path) -> Result<(), String> {
    for comp in rel.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                return Err(format!(
                    "refusing archive entry with an unsafe path: {}",
                    rel.display()
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Normalize an extracted unix mode: preserve the execute bit, drop the rest
/// (setuid/setgid never survive). No-op off unix.
fn normalize_mode(path: &Path, mode: Option<u32>) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Some(mode) = mode else { return };
        // Directories and executables keep a sane 0o755; everything else 0o644.
        let normalized = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
        if let Ok(md) = fs::symlink_metadata(path) {
            if !md.file_type().is_symlink() {
                let _ = fs::set_permissions(path, fs::Permissions::from_mode(normalized));
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

fn extract_zip(data: &[u8], dest: &Path) -> Result<(), String> {
    let mut zip = zip::ZipArchive::new(io::Cursor::new(data))
        .map_err(|e| format!("invalid zip archive: {e}"))?;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| format!("bad zip entry {i}: {e}"))?;
        let Some(rel) = entry.enclosed_name() else {
            return Err(format!(
                "refusing zip entry with an unsafe path: {}",
                entry.name()
            ));
        };
        reject_unsafe_path(&rel)?;
        if entry.is_symlink() {
            return Err(format!(
                "refusing symlink entry in archive: {}",
                entry.name()
            ));
        }
        let out = dest.join(&rel);
        if entry.is_dir() {
            fs::create_dir_all(&out)
                .map_err(|e| format!("cannot create {}: {e}", out.display()))?;
            continue;
        }
        let mode = entry.unix_mode();
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let mut f =
            fs::File::create(&out).map_err(|e| format!("cannot write {}: {e}", out.display()))?;
        io::copy(&mut entry, &mut f)
            .map_err(|e| format!("cannot extract {}: {e}", out.display()))?;
        normalize_mode(&out, mode);
    }
    Ok(())
}

fn extract_tar_gz(data: &[u8], dest: &Path) -> Result<(), String> {
    let gz = flate2::read::GzDecoder::new(io::Cursor::new(data));
    let mut archive = tar::Archive::new(gz);
    // Do not honor archived modes directly; normalize below instead.
    archive.set_preserve_permissions(false);
    archive.set_preserve_mtime(false);
    for entry in archive
        .entries()
        .map_err(|e| format!("invalid tar archive: {e}"))?
    {
        let mut entry = entry.map_err(|e| format!("bad tar entry: {e}"))?;
        let rel = entry
            .path()
            .map_err(|e| format!("bad tar entry path: {e}"))?
            .into_owned();
        reject_unsafe_path(&rel)?;
        let mode = entry.header().mode().ok();
        let unpacked = entry
            .unpack_in(dest)
            .map_err(|e| format!("cannot extract {}: {e}", rel.display()))?;
        if !unpacked {
            return Err(format!(
                "refusing tar entry that would unpack outside the destination: {}",
                rel.display()
            ));
        }
        normalize_mode(&dest.join(&rel), mode);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "inka-archive-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_zip(path: &Path, build: impl FnOnce(&mut zip::ZipWriter<fs::File>)) {
        let f = fs::File::create(path).unwrap();
        let mut w = zip::ZipWriter::new(f);
        build(&mut w);
        w.finish().unwrap();
    }

    #[test]
    fn zip_unpacks_flat_and_nested_entries() {
        let dir = scratch("zip-flat");
        let zip_path = dir.join("toolchain.zip");
        write_zip(&zip_path, |w| {
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("inka", opts).unwrap();
            w.write_all(b"bin").unwrap();
            w.start_file("nested/inka-launcher", opts).unwrap();
            w.write_all(b"launcher").unwrap();
        });
        let out = dir.join("out");
        extract("toolchain.zip", &zip_path, &out).unwrap();
        assert_eq!(fs::read(out.join("inka")).unwrap(), b"bin");
        assert_eq!(
            fs::read(out.join("nested/inka-launcher")).unwrap(),
            b"launcher"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn zip_rejects_symlink_entries() {
        let dir = scratch("zip-symlink");
        let zip_path = dir.join("evil.zip");
        write_zip(&zip_path, |w| {
            let opts = zip::write::SimpleFileOptions::default();
            w.add_symlink("link", "/etc/passwd", opts).unwrap();
        });
        let out = dir.join("out");
        let err = extract("evil.zip", &zip_path, &out).unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn zip_rejects_parent_dir_entries() {
        let dir = scratch("zip-traversal");
        let zip_path = dir.join("evil.zip");
        write_zip(&zip_path, |w| {
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("../escape", opts).unwrap();
            w.write_all(b"x").unwrap();
        });
        let out = dir.join("out");
        assert!(extract("evil.zip", &zip_path, &out).is_err());
        assert!(!dir.join("escape").exists(), "must not escape the dest");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tar_rejects_parent_dir_entries() {
        let dir = scratch("tar-traversal");
        let tar_path = dir.join("evil.tar.gz");
        {
            let f = fs::File::create(&tar_path).unwrap();
            let enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
            let mut b = tar::Builder::new(enc);
            let mut header = tar::Header::new_gnu();
            header.set_size(1);
            header.set_mode(0o644);
            // Write the raw name field to bypass the builder's own `..` check;
            // the extractor must still refuse it.
            header.as_mut_bytes()[..9].copy_from_slice(b"../escape");
            header.set_cksum();
            b.append(&header, &b"x"[..]).unwrap();
            b.finish().unwrap();
        }
        let out = dir.join("out");
        assert!(extract("evil.tar.gz", &tar_path, &out).is_err());
        assert!(!dir.join("escape").exists(), "must not escape the dest");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn tar_preserves_exec_but_drops_setuid() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("tar-modes");
        let tar_path = dir.join("a.tar.gz");
        {
            let f = fs::File::create(&tar_path).unwrap();
            let enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
            let mut b = tar::Builder::new(enc);
            // 0o4755 = setuid + rwxr-xr-x.
            for (name, mode) in [("run", 0o4755u32), ("data", 0o644u32)] {
                let mut header = tar::Header::new_gnu();
                header.set_size(1);
                header.set_mode(mode);
                header.set_cksum();
                b.append_data(&mut header, name, &b"x"[..]).unwrap();
            }
            b.finish().unwrap();
        }
        let out = dir.join("out");
        extract("a.tar.gz", &tar_path, &out).unwrap();
        let run = fs::metadata(out.join("run")).unwrap().permissions().mode();
        assert_eq!(run & 0o7777, 0o755, "exec preserved, setuid dropped");
        let data = fs::metadata(out.join("data")).unwrap().permissions().mode();
        assert_eq!(data & 0o7777, 0o644);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn zip_preserves_exec_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("zip-mode");
        let zip_path = dir.join("t.zip");
        write_zip(&zip_path, |w| {
            let opts = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
            w.start_file("inka", opts).unwrap();
            w.write_all(b"bin").unwrap();
        });
        let out = dir.join("out");
        extract("t.zip", &zip_path, &out).unwrap();
        let mode = fs::metadata(out.join("inka")).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "exec bit preserved");
        let _ = fs::remove_dir_all(&dir);
    }
}

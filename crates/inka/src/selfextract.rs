//! Self-extracting app directories.
//!
//! Adapted from Deno's `make_self_extracting_dir` / `write_tar_compressed`
//! (`cli/tools/desktop.rs`, MIT, Copyright (c) the Deno authors). The packaged
//! app dir is replaced by a thin one holding a gzip-compressed tar of the real
//! tree plus a launcher that extracts to a per-user cache on first run and
//! execs the real app. gzip (not xz/zstd) keeps the dependency set small and is
//! supported by the `tar` shipped with Windows.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Reject an unsupported `--compress` format before packaging.
pub(crate) fn validate_format(format: &str) -> Result<(), String> {
    match format {
        "gzip" | "gz" => Ok(()),
        other => Err(format!(
            "unknown --compress format '{other}' (supported: gzip)"
        )),
    }
}

/// Replace the app dir `dir` with a thin self-extracting one. `app_name` (the
/// backend/exe name, which may differ from the output dir name) and `id` (the
/// reverse-DNS bundle id) are used for the payload's top-level entry and the
/// per-user extraction path. `windows` picks the `.bat` vs POSIX launcher.
pub(crate) fn make_self_extracting(
    dir: &Path,
    app_name: &str,
    id: &str,
    windows: bool,
) -> Result<(), String> {
    let app_name = app_name.to_string();
    let parent = match dir.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };

    // Move the real tree into a sibling staging dir, then rebuild a thin dir.
    let staging = parent.join(format!(
        ".selfextract-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)
        .map_err(|e| format!("cannot create {}: {e}", staging.display()))?;
    let inner = staging.join(&app_name);
    fs::rename(dir, &inner).map_err(|e| format!("cannot move {}: {e}", dir.display()))?;
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    let result = (|| {
        let payload_name = "payload.tar.gz";
        let payload = dir.join(payload_name);
        write_tar_gz(&staging, &app_name, &payload)?;
        let hash = payload_hash(&payload)?;
        let launcher = if windows {
            windows_launcher(&app_name, id, &hash, payload_name)
        } else {
            unix_launcher(&app_name, id, &hash, payload_name)
        };
        let path = dir.join(&launcher.0);
        fs::write(&path, launcher.1)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        if !windows {
            crate::platform::set_exec(&path)
                .map_err(|e| format!("cannot mark {} executable: {e}", path.display()))?;
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&staging);
    result
}

/// `(launcher file name, contents)`.
fn windows_launcher(app_name: &str, id: &str, hash: &str, payload_name: &str) -> (String, String) {
    let contents = format!(
        "@echo off\r\n\
         setlocal\r\n\
         set \"DIR=%~dp0\"\r\n\
         set \"DEST=%LOCALAPPDATA%\\{id}\\{hash}\"\r\n\
         if not exist \"%DEST%\\{app_name}\\{app_name}.exe\" (\r\n\
         \u{20} mkdir \"%DEST%\" 2>nul\r\n\
         \u{20} tar -xf \"%DIR%{payload_name}\" -C \"%DEST%\"\r\n\
         )\r\n\
         \"%DEST%\\{app_name}\\{app_name}.exe\" %*\r\n",
    );
    (format!("{app_name}.bat"), contents)
}

/// `(launcher file name, contents)`.
fn unix_launcher(app_name: &str, id: &str, hash: &str, payload_name: &str) -> (String, String) {
    let contents = format!(
        "#!/bin/sh\n\
         set -e\n\
         DIR=\"$(cd \"$(dirname \"$0\")\" && pwd)\"\n\
         DEST=\"${{XDG_DATA_HOME:-$HOME/.local/share}}/{id}/{hash}\"\n\
         APP=\"$DEST/{app_name}\"\n\
         if [ ! -x \"$APP/{app_name}\" ]; then\n\
         \u{20} mkdir -p \"$DEST\"\n\
         \u{20} tar -xf \"$DIR/{payload_name}\" -C \"$DEST\"\n\
         fi\n\
         exec \"$APP/{app_name}\" \"$@\"\n",
    );
    (app_name.to_string(), contents)
}

/// Tar `parent/entry_name` (preserving symlinks) into `dest`, gzip-compressed.
fn write_tar_gz(parent: &Path, entry_name: &str, dest: &Path) -> Result<(), String> {
    let mut tar_buf = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);
        builder.follow_symlinks(false);
        builder
            .append_dir_all(entry_name, parent.join(entry_name))
            .map_err(|e| format!("cannot archive {entry_name}: {e}"))?;
        builder
            .finish()
            .map_err(|e| format!("cannot finish archive: {e}"))?;
    }
    let file =
        fs::File::create(dest).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    enc.write_all(&tar_buf)
        .map_err(|e| format!("cannot compress payload: {e}"))?;
    enc.finish()
        .map_err(|e| format!("cannot finalize payload: {e}"))?;
    Ok(())
}

/// Short, stable cache key from the payload bytes (bumps on content change).
fn payload_hash(payload: &Path) -> Result<String, String> {
    let digest = crate::sha256_file(payload)?;
    Ok(digest[..16].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "inka-selfextract-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn format_validation() {
        assert!(validate_format("gzip").is_ok());
        assert!(validate_format("gz").is_ok());
        assert!(validate_format("zstd").is_err());
        assert!(validate_format("xz").is_err());
    }

    #[test]
    fn tar_gz_roundtrips_the_tree() {
        let base = scratch("tar");
        let stage = base.join("stage");
        let app = stage.join("Acme");
        fs::create_dir_all(app.join("locales")).unwrap();
        fs::write(app.join("Acme.exe"), b"exe").unwrap();
        fs::write(app.join("locales/en-US.pak"), b"pak").unwrap();
        let payload = base.join("payload.tar.gz");
        write_tar_gz(&stage, "Acme", &payload).unwrap();

        let f = fs::File::open(&payload).unwrap();
        let gz = flate2::read::GzDecoder::new(f);
        let mut archive = tar::Archive::new(gz);
        let mut names: Vec<String> = archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "Acme/",
                "Acme/Acme.exe",
                "Acme/locales",
                "Acme/locales/en-US.pak",
            ]
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn make_self_extracting_replaces_dir_with_launcher_and_payload() {
        // The output dir name ("dist-out") differs from the app name ("Acme"),
        // which must drive the launcher and payload entry — not the dir name.
        let base = scratch("dir");
        let app = base.join("dist-out");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("Acme"), b"bin").unwrap();
        fs::write(app.join("Acme.so"), b"shim").unwrap();

        make_self_extracting(&app, "Acme", "com.inka.desktop.acme", false).unwrap();
        let payload = app.join("payload.tar.gz");
        assert!(payload.is_file());
        assert!(app.join("Acme").is_file(), "POSIX launcher present");
        // The real binary is no longer at the top level (only inside the tar).
        assert!(!app.join("Acme.so").exists(), "payload replaced the tree");
        let hash = payload_hash(&payload).unwrap();

        let launcher = fs::read_to_string(app.join("Acme")).unwrap();
        assert!(launcher.starts_with("#!/bin/sh\n"));
        assert!(launcher.contains("payload.tar.gz"));
        assert!(launcher.contains(&hash), "{launcher}");
        assert!(launcher.contains("/com.inka.desktop.acme/"), "{launcher}");

        // The payload's top-level entry is the app name, so extraction yields
        // `<DEST>/Acme/Acme`.
        let f = fs::File::open(&payload).unwrap();
        let gz = flate2::read::GzDecoder::new(f);
        let mut archive = tar::Archive::new(gz);
        assert!(archive
            .entries()
            .unwrap()
            .any(|e| e.unwrap().path().unwrap() == Path::new("Acme/Acme")));
        let _ = fs::remove_dir_all(&base);
    }
}

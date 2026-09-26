//! Deep-link (custom URL scheme) registration.
//!
//! Vendored and adapted from Deno's `cli/tools/desktop.rs`
//! (`validate_url_scheme` / `register_deep_links*`, MIT, Copyright (c) the Deno
//! authors). Only the declarative registration is written into the bundle;
//! delivering an opened URL into the running app is a separate concern.

use std::fs;
use std::path::Path;

/// Validate every scheme up front so the build fails before packaging.
pub(crate) fn validate_schemes(schemes: &[String]) -> Result<(), String> {
    for scheme in schemes {
        validate_url_scheme(scheme)?;
    }
    Ok(())
}

/// Register `schemes` into the app bundle for the current target.
pub(crate) fn register(bundle_dir: &Path, schemes: &[String]) -> Result<(), String> {
    if schemes.is_empty() {
        return Ok(());
    }
    if cfg!(windows) {
        register_windows(bundle_dir, schemes)
    } else {
        register_linux(bundle_dir, schemes)
    }
}

/// Validate a deep-link URL scheme per RFC 3986; reject reserved schemes.
fn validate_url_scheme(scheme: &str) -> Result<(), String> {
    let reserved = ["http", "https", "file", "ftp", "ws", "wss"];
    let bail = |reason: &str| Err(format!("invalid deep-link scheme {scheme:?}: {reason}"));
    match scheme.chars().next() {
        None => return bail("scheme is empty"),
        Some(c) if !c.is_ascii_alphabetic() => {
            return bail("scheme must start with an ASCII letter");
        }
        _ => {}
    }
    if !scheme
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return bail("scheme may only contain letters, digits, '+', '-', and '.'");
    }
    if reserved.contains(&scheme) {
        return bail("scheme is reserved and cannot be used as a deep link");
    }
    Ok(())
}

/// Windows has no in-bundle declarative protocol registration, so write a
/// `register-deep-links.bat` that adds `HKCU\Software\Classes\<scheme>` keys
/// pointing back at the launcher (`%~dp0<App>.exe`).
fn register_windows(bundle_dir: &Path, schemes: &[String]) -> Result<(), String> {
    let launcher = bundle_dir
        .file_name()
        .map(|n| format!("{}.exe", n.to_string_lossy()))
        .unwrap_or_else(|| "launcher.exe".to_string());

    let mut script = String::from("@echo off\r\nsetlocal\r\n");
    for scheme in schemes {
        script.push_str(&format!(
            "reg add \"HKCU\\Software\\Classes\\{scheme}\" /ve /d \"URL:{scheme}\" /f\r\n\
             reg add \"HKCU\\Software\\Classes\\{scheme}\" /v \"URL Protocol\" /d \"\" /f\r\n\
             reg add \"HKCU\\Software\\Classes\\{scheme}\\shell\\open\\command\" /ve /d \"\\\"%~dp0{launcher}\\\" \\\"%%1\\\"\" /f\r\n",
        ));
    }
    script.push_str("endlocal\r\n");

    let path = bundle_dir.join("register-deep-links.bat");
    fs::write(&path, script).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Linux registers handlers through the `.desktop` entry: add
/// `x-scheme-handler/<scheme>;` MIME types and ensure `Exec=` forwards the URL
/// via a `%u` field code.
fn register_linux(bundle_dir: &Path, schemes: &[String]) -> Result<(), String> {
    let desktop_file = fs::read_dir(bundle_dir)
        .map_err(|e| format!("cannot read {}: {e}", bundle_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|e| e == "desktop"))
        .ok_or_else(|| format!("no .desktop file found in {}", bundle_dir.display()))?;

    let contents = fs::read_to_string(&desktop_file)
        .map_err(|e| format!("cannot read {}: {e}", desktop_file.display()))?;
    let mime = schemes
        .iter()
        .map(|s| format!("x-scheme-handler/{s};"))
        .collect::<String>();

    let mut out = String::with_capacity(contents.len() + mime.len() + 16);
    let mut wrote_mime = false;
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("Exec=") {
            if rest.contains("%u") || rest.contains("%U") {
                out.push_str(line);
            } else {
                out.push_str(&format!("Exec={} %u", rest.trim_end()));
            }
            out.push('\n');
        } else if let Some(rest) = line.strip_prefix("MimeType=") {
            out.push_str("MimeType=");
            out.push_str(rest.trim_end());
            if !rest.trim_end().ends_with(';') && !rest.trim_end().is_empty() {
                out.push(';');
            }
            out.push_str(&mime);
            out.push('\n');
            wrote_mime = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !wrote_mime {
        out.push_str(&format!("MimeType={mime}\n"));
    }

    fs::write(&desktop_file, out)
        .map_err(|e| format!("cannot write {}: {e}", desktop_file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "inka-links-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn scheme_validation() {
        assert!(validate_url_scheme("acme").is_ok());
        assert!(validate_url_scheme("acme-mail2").is_ok());
        assert!(validate_url_scheme("a.b+c-d").is_ok());
        assert!(validate_url_scheme("").is_err());
        assert!(validate_url_scheme("1acme").is_err());
        assert!(validate_url_scheme("a b").is_err());
        assert!(validate_url_scheme("a/b").is_err());
        for reserved in ["http", "https", "file", "ftp", "ws", "wss"] {
            assert!(validate_url_scheme(reserved).is_err(), "{reserved}");
        }
    }

    #[test]
    fn windows_bat_has_registration_keys() {
        let dir = scratch("bat");
        let app = dir.join("Acme");
        fs::create_dir_all(&app).unwrap();
        register_windows(&app, &["acme".to_string(), "acme-mail".to_string()]).unwrap();
        let bat = fs::read_to_string(app.join("register-deep-links.bat")).unwrap();
        assert!(bat.starts_with("@echo off\r\n"));
        assert!(bat.contains("HKCU\\Software\\Classes\\acme"));
        assert!(bat.contains("URL Protocol"));
        assert!(bat.contains("%~dp0Acme.exe"));
        assert!(bat.ends_with("endlocal\r\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn linux_desktop_merges_mime_and_exec() {
        let dir = scratch("desktop");
        let app = dir.join("Acme");
        fs::create_dir_all(&app).unwrap();
        let entry = app.join("com.acme.mail.desktop");
        fs::write(
            &entry,
            "[Desktop Entry]\nType=Application\nName=Acme\nExec=Acme\nIcon=AppIcon\nCategories=Utility;\n",
        )
        .unwrap();
        register_linux(&app, &["acme".to_string()]).unwrap();
        let out = fs::read_to_string(&entry).unwrap();
        assert!(out.contains("Exec=Acme %u\n"), "{out}");
        assert!(out.contains("MimeType=x-scheme-handler/acme;\n"), "{out}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn linux_desktop_merges_into_existing_mime_and_exec() {
        let dir = scratch("desktop-merge");
        let app = dir.join("Acme");
        fs::create_dir_all(&app).unwrap();
        let entry = app.join("com.acme.mail.desktop");
        fs::write(
            &entry,
            "[Desktop Entry]\nExec=Acme %u\nMimeType=text/plain;\n",
        )
        .unwrap();
        register_linux(&app, &["acme".to_string()]).unwrap();
        let out = fs::read_to_string(&entry).unwrap();
        // No duplicate %u, and the MIME list is appended, not replaced.
        assert_eq!(out.matches("%u").count(), 1, "{out}");
        assert!(
            out.contains("MimeType=text/plain;x-scheme-handler/acme;\n"),
            "{out}"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

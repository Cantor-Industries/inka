// Script-installer output for `inka desktop --installer`.
//
// Produces a `<App>.tar.gz` of the app-specific files, a standalone
// `<App>.install.sh`, and a `.sha256` sidecar. The script (embedded in the
// tarball too) reuses the shared engine/CEF runtime when present and downloads
// the exact versions from the inka release it was built against otherwise. This
// is what makes the tarball portable: unlike the runnable app dir, it carries
// no absolute symlinks into the builder's home.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::channel;
use crate::desktop::{laufey_archive_name, LAUFEY_SUMS, LAUFEY_TARGET, LAUFEY_VERSION};

/// The install script template (placeholders are `{{NAME}}`).
const TEMPLATE: &str = include_str!("../assets/install-app.sh");

/// Everything the generated installer needs to know about the app.
pub(crate) struct Spec<'a> {
    pub app_name: &'a str,
    pub app_id: &'a str,
    pub backend: &'a str,
    /// The exact engine tuple the app requires (`runtime-version`).
    pub runtime: &'a str,
    pub app_version: Option<&'a str>,
    /// The app's own download base (for fetching its tarball), if configured.
    pub app_base: Option<&'a str>,
    /// Override for the inka release base serving the engine/CEF runtime.
    pub engine_base: Option<&'a str>,
}

/// The files written for an installer build.
pub(crate) struct Outputs {
    pub tarball: PathBuf,
    pub script: PathBuf,
    pub sha256: PathBuf,
}

/// Single-quote a value for safe embedding in the generated `sh` script.
/// Validated inputs never contain a single quote, but escape defensively.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The inka release base the installer downloads the shared runtime from:
/// an explicit override, else the exact baked tag, else `releases/latest`.
fn inka_release_base(override_base: Option<&str>) -> String {
    if let Some(b) = override_base.map(str::trim).filter(|s| !s.is_empty()) {
        return b.to_string();
    }
    if let Ok(b) = env::var("INKA_RELEASE_BASE") {
        if !b.trim().is_empty() {
            return b;
        }
    }
    let repo = env::var("INKA_REPO")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Cantor-Industries/inka".to_string());
    match channel::build_tag() {
        Some(tag) => format!("https://github.com/{repo}/releases/download/{tag}"),
        None => format!("https://github.com/{repo}/releases/latest/download"),
    }
}

fn laufey_sha(archive: &str) -> Option<&'static str> {
    LAUFEY_SUMS
        .iter()
        .find(|(n, _)| *n == archive)
        .map(|(_, h)| *h)
}

/// Render the installer script for `spec`.
pub(crate) fn render(spec: &Spec) -> Result<String, String> {
    let (cef_archive, cef_sha) = if spec.backend == "cef" {
        let archive = laufey_archive_name("cef");
        let sha =
            laufey_sha(&archive).ok_or_else(|| format!("no pinned checksum for {archive}"))?;
        (archive, sha.to_string())
    } else {
        (String::new(), String::new())
    };
    let base = inka_release_base(spec.engine_base);

    let out = TEMPLATE
        .replace("{{APP_NAME}}", &sh_quote(spec.app_name))
        .replace("{{APP_ID}}", &sh_quote(spec.app_id))
        .replace("{{BACKEND}}", &sh_quote(spec.backend))
        .replace("{{RUNTIME}}", &sh_quote(spec.runtime))
        .replace("{{LAUFEY_VERSION}}", &sh_quote(LAUFEY_VERSION))
        .replace("{{TARGET}}", &sh_quote(LAUFEY_TARGET))
        .replace("{{APP_VERSION}}", &sh_quote(spec.app_version.unwrap_or("")))
        .replace("{{APP_BASE}}", &sh_quote(spec.app_base.unwrap_or("")))
        .replace("{{INKA_BASE}}", &sh_quote(&base))
        .replace("{{CEF_ARCHIVE}}", &sh_quote(&cef_archive))
        .replace("{{CEF_SHA256}}", &sh_quote(&cef_sha));
    Ok(out)
}

/// Stage the app-specific files from the runnable app dir `out`, embed the
/// installer, and write `<App>.tar.gz` / `<App>.install.sh` / `.sha256` beside
/// `out`.
pub(crate) fn build(out: &Path, spec: &Spec) -> Result<Outputs, String> {
    let parent = match out.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let staging = parent.join(format!(".{}.installer", spec.app_name));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)
        .map_err(|e| format!("cannot create {}: {e}", staging.display()))?;

    // App-specific real files only: the launcher, the shim+payload, the runtime
    // marker, the desktop entry and (optionally) the icon. CEF runtime files are
    // provisioned by the installer, so they are never shipped here.
    let entries = [
        spec.app_name.to_string(),
        format!("{}.so", spec.app_name),
        "runtime-version".to_string(),
        format!("{}.desktop", spec.app_id),
    ];
    for name in &entries {
        let src = out.join(name);
        if !src.is_file() {
            let _ = fs::remove_dir_all(&staging);
            return Err(format!("missing app file for installer: {}", src.display()));
        }
        fs::copy(&src, staging.join(name))
            .map_err(|e| format!("cannot copy {}: {e}", src.display()))?;
    }
    if out.join("AppIcon.png").is_file() {
        fs::copy(out.join("AppIcon.png"), staging.join("AppIcon.png"))
            .map_err(|e| format!("cannot copy icon: {e}"))?;
    }

    let script = render(spec)?;
    fs::write(staging.join("install.sh"), &script)
        .map_err(|e| format!("cannot write install.sh: {e}"))?;

    // Pack the staging contents at the archive root.
    let tarball = parent.join(format!("{}.tar.gz", spec.app_name));
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&tarball)
        .arg("-C")
        .arg(&staging)
        .arg(".")
        .status()
        .map_err(|e| format!("could not run tar: {e}"))?;
    let _ = fs::remove_dir_all(&staging);
    if !status.success() {
        return Err(format!("tar exited with {status}"));
    }

    // Standalone script (for `curl | sh` / mirrors) and the tarball checksum.
    let standalone = parent.join(format!("{}.install.sh", spec.app_name));
    fs::write(&standalone, &script)
        .map_err(|e| format!("cannot write {}: {e}", standalone.display()))?;
    set_exec(&standalone);
    let sha256 = sha_path(&tarball);
    fs::write(&sha256, format!("{}\n", crate::sha256_file(&tarball)?))
        .map_err(|e| format!("cannot write {}: {e}", sha256.display()))?;

    Ok(Outputs {
        tarball,
        script: standalone,
        sha256,
    })
}

fn sha_path(tarball: &Path) -> PathBuf {
    PathBuf::from(format!("{}.sha256", tarball.display()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn scratch(tag: &str) -> PathBuf {
        let d = env::temp_dir().join(format!(
            "inka-installer-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn spec<'a>() -> Spec<'a> {
        Spec {
            app_name: "Demo",
            app_id: "com.inka.desktop.demo",
            backend: "cef",
            runtime: "0.267.2-beta.2",
            app_version: Some("1.2.3"),
            app_base: None,
            engine_base: Some("https://example.test/dl"),
        }
    }

    #[test]
    fn render_embeds_fields_and_quotes() {
        let s = render(&spec()).unwrap();
        assert!(s.contains("APP_NAME='Demo'"));
        assert!(s.contains("APP_ID='com.inka.desktop.demo'"));
        assert!(s.contains("BACKEND='cef'"));
        assert!(s.contains("RUNTIME='0.267.2-beta.2'"));
        assert!(s.contains("APP_VERSION='1.2.3'"));
        assert!(s.contains("INKA_BASE='https://example.test/dl'"));
        assert!(s.contains("CEF_ARCHIVE='laufey-cef-x86_64-unknown-linux-gnu.tar.gz'"));
        // The pinned laufey checksum is embedded (64 hex chars).
        let sha = laufey_sha(&laufey_archive_name("cef")).unwrap();
        assert!(s.contains(&format!("CEF_SHA256='{sha}'")));
        assert!(!s.contains("{{"), "no unsubstituted placeholders");
        // The script must end with an explicit success: a trailing conditional
        // (e.g. `--no-modify-path`) must not make a successful install exit 1.
        assert!(
            s.trim_end().ends_with("exit 0"),
            "installer must end with `exit 0`"
        );
    }

    #[test]
    fn render_non_cef_leaves_cef_fields_empty() {
        let mut sp = spec();
        sp.backend = "webview";
        let s = render(&sp).unwrap();
        assert!(s.contains("BACKEND='webview'"));
        assert!(s.contains("CEF_ARCHIVE=''"));
        assert!(s.contains("CEF_SHA256=''"));
    }

    #[cfg(unix)]
    #[test]
    fn build_packs_app_specific_files_only() {
        let base = scratch("pack");
        let out = base.join("Demo");
        fs::create_dir_all(&out).unwrap();
        for f in [
            "Demo",
            "Demo.so",
            "runtime-version",
            "com.inka.desktop.demo.desktop",
        ] {
            fs::write(out.join(f), b"x").unwrap();
        }
        // A CEF symlink in the runnable dir must NOT be shipped.
        fs::write(out.join("libcef.so"), b"cef").unwrap();
        std::os::unix::fs::symlink(out.join("libcef.so"), out.join("libcef-link.so")).unwrap();

        let outputs = build(&out, &spec()).unwrap();

        assert!(outputs.tarball.is_file());
        assert!(outputs.script.is_file());
        assert_eq!(
            outputs.sha256,
            PathBuf::from(format!("{}.sha256", outputs.tarball.display()))
        );
        assert!(outputs.sha256.is_file());

        // Inspect the archive members.
        let list = Command::new("tar")
            .arg("-tzf")
            .arg(&outputs.tarball)
            .output()
            .unwrap();
        let names = String::from_utf8_lossy(&list.stdout);
        for want in [
            "install.sh",
            "Demo",
            "Demo.so",
            "runtime-version",
            "com.inka.desktop.demo.desktop",
        ] {
            assert!(names.contains(want), "missing {want} in {names}");
        }
        assert!(
            !names.contains("libcef"),
            "CEF must not be shipped: {names}"
        );
        assert!(!names.contains("AppIcon.png"));
        let _ = fs::remove_dir_all(&base);
    }
}

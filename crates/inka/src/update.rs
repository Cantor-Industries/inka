// inka update: reconcile the toolchain and the shared runtime tuple with the
// newest published release (or install an explicit runtime tuple).
//
//   inka update [<version>] [--from <dir-or-url>] [--sha256 <hex>]
//                           [--insecure] [--home <dir>]
//                           [--no-toolchain | --toolchain-only]
//                           [--no-runtime]
//
// No <version>: fetch <base>/versions.json, self-update the toolchain when an
// installer-managed one is present, install the runtime when it is behind.
// With <version>: install that exact runtime tuple (pinned/offline; CI,
// containers).
//
// Component policy: download only when missing or a newer version is published;
// never downgrade; older runtime tuples are left in place (artifacts roll
// forward to the newest satisfying tuple).
//
// The base defaults to the GitHub "latest release" asset base; override with
// --from, $INKA_RELEASE_BASE, or $INKA_RT_SOURCE.

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::help::{self, Mode};
use crate::ui;
use crate::{
    download_file, fetch_sidecar, fetch_text, parse_version, sha256_file, Version, FILENAME_PREFIX,
    FILENAME_SUFFIX,
};

pub(crate) const DEFAULT_CHANNEL: &str =
    "https://github.com/Cantor-Industries/inka/releases/latest/download";

fn fail(msg: &str) -> ! {
    ui::log_error(msg);
    std::process::exit(1);
}

/// A short unpredictable suffix for temp names (urandom, fallback pid+time).
fn random_suffix() -> String {
    use std::io::Read;
    let mut buf = [0u8; 8];
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        if f.read_exact(&mut buf).is_ok() {
            return buf.iter().map(|b| format!("{b:02x}")).collect();
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}{:x}", std::process::id(), nanos)
}

/// A remote-supplied asset name must be a plain basename: non-empty, no
/// directory separators, no `..`, no NUL. Guards `base.join(name)` and
/// `target_dir.join(name)` against traversal from a hostile `versions.json`.
fn valid_asset_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
        && Path::new(name).components().count() == 1
}

/// Removes a temp directory on drop, so failures don't leave junk behind.
struct TempDir(PathBuf);
impl TempDir {
    /// Create a fresh, exclusive temp dir (retrying on a name collision).
    fn create(prefix: &str) -> Result<TempDir, String> {
        let base = env::temp_dir();
        for _ in 0..8 {
            let p = base.join(format!(
                "{prefix}{}-{}",
                std::process::id(),
                random_suffix()
            ));
            match fs::create_dir(&p) {
                Ok(()) => {
                    // Stamp the owner so a crashed update's tree can be reaped
                    // by the launcher's `.inka-owner` sweeper.
                    let _ = fs::write(p.join(".inka-owner"), std::process::id().to_string());
                    return Ok(TempDir(p));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("cannot create {}: {e}", p.display())),
            }
        }
        Err(format!("could not create a unique {prefix} temp dir"))
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn resolve_base(from: Option<String>) -> String {
    from.or_else(|| env::var("INKA_RELEASE_BASE").ok())
        .or_else(|| env::var("INKA_RT_SOURCE").ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_CHANNEL.to_string())
}

/// Where this update writes the engine: `--home`, else `INKA_RUNTIME_HOME`, else
/// the system dir when root, else the per-user XDG runtime dir.
fn target_dir(home: Option<&str>) -> PathBuf {
    match home {
        Some(h) => PathBuf::from(h),
        None => crate::default_install_dir(),
    }
}

fn ensure_dir(dir: &Path) {
    fs::create_dir_all(dir).unwrap_or_else(|e| {
        ui::log_error(format!("cannot create {}: {e}", dir.display()));
        std::process::exit(1);
    });
}

/// Which components to reconcile. Defaults to all.
#[derive(Clone, Copy)]
struct Components {
    runtime: bool,
}

impl Default for Components {
    fn default() -> Self {
        Self { runtime: true }
    }
}

/// Toolchain self-update policy.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolchainMode {
    /// Self-update when an installer-managed toolchain (`VERSION` marker) is present.
    Auto,
    /// Only update the toolchain; skip the engine.
    Only,
    /// Never touch the toolchain.
    Skip,
}

pub(crate) fn cmd_update(args: &[String]) {
    let mut version = None;
    let mut from = None;
    let mut sha256 = None;
    let mut insecure = false;
    let mut home = None;
    let mut toolchain = ToolchainMode::Auto;
    let mut components = Components::default();

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--from" => from = it.next().cloned(),
            "--sha256" => sha256 = it.next().cloned(),
            "--home" => home = it.next().cloned(),
            "--insecure" => insecure = true,
            "--no-toolchain" => toolchain = ToolchainMode::Skip,
            "--toolchain-only" => toolchain = ToolchainMode::Only,
            "--no-runtime" => components.runtime = false,
            "-q" | "--quiet" | "-v" | "--verbose" => {
                ui::apply_verbosity_flag(a);
            }
            "-h" => {
                help::print(help::update(), Mode::Short);
                std::process::exit(0);
            }
            "--help" => {
                help::print(help::update(), Mode::Long);
                std::process::exit(0);
            }
            other => {
                if version.is_none() {
                    version = Some(other.to_string());
                } else {
                    fail("inka update takes at most one <version>");
                }
            }
        }
    }

    let base = resolve_base(from);
    ui::title("update");
    match version {
        Some(v) => update_pinned(
            &base,
            &v,
            sha256,
            insecure,
            home.as_deref(),
            &components,
            toolchain,
        ),
        None => update_latest(&base, insecure, home.as_deref(), &components, toolchain),
    }
}

// ---- toolchain self-update --------------------------------------------------

/// Directory holding the running `inka` binary. Self-update only proceeds when a
/// `VERSION` marker is present, so a dev build in `target/release` is never
/// clobbered by a published toolchain.
fn toolchain_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(Path::to_path_buf)
}

fn installed_toolchain_version(dir: &Path) -> String {
    fs::read_to_string(dir.join("VERSION"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Fetch `versions.json` and self-update the installer-managed toolchain when a
/// newer release is available. No-op (Ok) when there is no `VERSION` marker or
/// the release carries no `toolchain` metadata. Never downgrades.
fn update_toolchain(base: &str, insecure: bool) -> Result<bool, String> {
    let versions_text = fetch_text(base, "versions.json")
        .map_err(|e| format!("cannot read versions.json from {base}: {e}"))?;
    let v: Value = serde_json::from_str(&versions_text)
        .map_err(|e| format!("invalid versions.json from {base}: {e}"))?;
    update_toolchain_from(&v, base, insecure)
}

/// Toolchain self-update against an already-parsed `versions.json`.
fn update_toolchain_from(v: &Value, base: &str, insecure: bool) -> Result<bool, String> {
    let Some(dir) = toolchain_dir() else {
        return Ok(false);
    };
    if !dir.join("VERSION").is_file() {
        return Ok(false);
    }
    let installed = installed_toolchain_version(&dir);
    let Some(tc) = v.get("toolchain") else {
        return Ok(false);
    };
    let latest = tc.get("version").and_then(Value::as_str);
    let archive = tc.get("archive").and_then(Value::as_str);
    let (Some(latest), Some(archive)) = (latest, archive) else {
        return Ok(false);
    };
    if !valid_asset_name(archive) {
        return Err(format!(
            "versions.json names an invalid toolchain archive '{archive}'"
        ));
    }

    ui::section("Toolchain");
    if let (Some(i), Some(l)) = (parse_version(&installed), parse_version(latest)) {
        if i >= l {
            ui::ok(
                "toolchain",
                format!("{installed} is current (latest {latest})"),
            );
            return Ok(false);
        }
    } else if installed == latest {
        ui::ok(
            "toolchain",
            format!("{installed} is current (latest {latest})"),
        );
        return Ok(false);
    }

    ui::doing("toolchain", format!("updating {installed} -> {latest}"));
    let expected = match tc.get("sha256").and_then(Value::as_str) {
        Some(s) => Some(s.to_ascii_lowercase()),
        None => fetch_sidecar(base, archive)
            .map_err(|e| format!("failed to fetch {archive}.sha256: {e}"))?
            .map(normalize_sha),
    };

    let tmp = TempDir::create("inka-toolchain-")?;
    // Fixed staging name: the remote name is only used for the fetch URL.
    let archive_path = tmp.0.join("toolchain.tar.gz");
    download_file(base, archive, &archive_path)
        .map_err(|e| format!("failed to fetch {archive}: {e}"))?;

    let actual = sha256_file(&archive_path)?;
    match expected {
        Some(exp) if exp != actual => {
            return Err(format!(
                "checksum mismatch for {archive}: expected {exp}, actual {actual}"
            ))
        }
        None if !insecure => {
            return Err(format!(
                "no checksum for {archive}; publish a .sha256 sidecar or pass --insecure"
            ))
        }
        _ => {}
    }

    let mut cmd = Command::new("tar");
    cmd.args(["-xzf"])
        .arg(&archive_path)
        .args(["--no-same-owner", "--no-same-permissions", "-C"])
        .arg(&tmp.0);
    run_ok(&mut cmd, "tar extract")?;
    replace_toolchain(&tmp.0, &dir)?;
    fs::write(dir.join("VERSION"), format!("{latest}\n"))
        .map_err(|e| format!("cannot write {}: {e}", dir.join("VERSION").display()))?;
    ui::ok("toolchain", format!("installed {latest}"));
    Ok(true)
}
/// Replace the toolchain binaries in `dir` from an extracted archive in
/// `staging`. Binary replacement is an atomic rename over the running image
/// (Linux keeps the old inode until this process exits).
fn replace_toolchain(staging: &Path, dir: &Path) -> Result<(), String> {
    let nonce = format!("{}-{}", std::process::id(), random_suffix());
    // Stage every binary first: if one is missing or unwritable, nothing is
    // replaced yet and the previous toolchain stays usable.
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
    for f in ["inka", "inka-launcher"] {
        let src = staging.join(f);
        if !src.is_file() {
            cleanup_staged(&staged);
            return Err(format!("toolchain archive is missing '{f}'"));
        }
        let new = dir.join(format!(".{f}.new{nonce}"));
        if let Err(e) = fs::copy(&src, &new)
            .and_then(|_| fs::set_permissions(&new, fs::Permissions::from_mode(0o755)))
        {
            let _ = fs::remove_file(&new);
            cleanup_staged(&staged);
            return Err(format!("cannot stage {f}: {e}"));
        }
        staged.push((new, dir.join(f)));
    }
    // Activate: rename each staged file over its target (fast; unlikely to fail
    // once staging succeeded).
    for (new, dst) in &staged {
        fs::rename(new, dst).map_err(|e| format!("cannot replace {}: {e}", dst.display()))?;
    }
    Ok(())
}

fn cleanup_staged(staged: &[(PathBuf, PathBuf)]) {
    for (new, _) in staged {
        let _ = fs::remove_file(new);
    }
}

fn toolchain_warn(e: String) -> bool {
    ui::warn(format!("toolchain not updated: {e}"));
    ui::detail("the engine update continues; re-run install.sh to update the toolchain");
    false
}

/// Best-effort toolchain self-update used by both update paths. Returns whether
/// the toolchain was actually replaced.
fn maybe_update_toolchain(base: &str, insecure: bool, mode: ToolchainMode) -> bool {
    if mode == ToolchainMode::Skip {
        return false;
    }
    match update_toolchain(base, insecure) {
        Ok(changed) => changed,
        Err(e) => toolchain_warn(e),
    }
}

/// As `maybe_update_toolchain`, against an already-parsed `versions.json`.
fn maybe_update_toolchain_from(v: &Value, base: &str, insecure: bool, mode: ToolchainMode) -> bool {
    if mode == ToolchainMode::Skip {
        return false;
    }
    match update_toolchain_from(v, base, insecure) {
        Ok(changed) => changed,
        Err(e) => toolchain_warn(e),
    }
}

// ---- latest -----------------------------------------------------------------

#[derive(Debug, PartialEq)]
struct Actions {
    runtime: bool,
}

/// Decide whether the engine runtime is behind the latest release. A component
/// is only ever fetched when the installed version is strictly older (never
/// downgrade); a missing component is always fetched.
fn plan_actions(installed_runtime: Option<Version>, latest_runtime: Version) -> Actions {
    let runtime = installed_runtime.is_none_or(|i| i < latest_runtime);
    Actions { runtime }
}

fn update_latest(
    base: &str,
    insecure: bool,
    home: Option<&str>,
    components: &Components,
    toolchain: ToolchainMode,
) {
    let versions_text = match fetch_text(base, "versions.json") {
        Ok(s) => s,
        Err(e) => fail(&format!("cannot read versions.json from {base}: {e}")),
    };
    let v: Value = serde_json::from_str(&versions_text)
        .unwrap_or_else(|e| fail(&format!("invalid versions.json from {base}: {e}")));

    let mut changed = maybe_update_toolchain_from(&v, base, insecure, toolchain);
    if toolchain == ToolchainMode::Only {
        return;
    }

    let target = target_dir(home);
    // Prefer the runtime tuple version; fall back to `deno_runtime` for releases
    // published before the tuple was decoupled from the crate pin.
    let latest_runtime_s = v
        .get("runtime")
        .or_else(|| v.get("deno_runtime"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| fail("versions.json has no runtime"));
    let latest_runtime = parse_version(latest_runtime_s)
        .unwrap_or_else(|| fail(&format!("invalid runtime '{latest_runtime_s}'")));
    let runtime_sha = v.get("runtime_sha256").and_then(Value::as_str);

    let search = match home {
        Some(_) => vec![target.clone()],
        None => crate::runtime_search_dirs(),
    };
    let runtimes = crate::installed_parts_all(&search);
    let installed_runtime = runtimes.last().map(|(ver, _)| *ver);

    let actions = plan_actions(installed_runtime, latest_runtime);

    ensure_dir(&target);

    if components.runtime {
        ui::section("Runtimes");
        if actions.runtime {
            let name = format!("{FILENAME_PREFIX}{latest_runtime}{FILENAME_SUFFIX}");
            install_file(
                base,
                &name,
                &target,
                runtime_sha.map(str::to_string),
                insecure,
            )
            .unwrap_or_else(|e| fail(&e));
            changed = true;
        } else if let Some(i) = installed_runtime {
            ui::ok(
                "runtime",
                format!("{i} is current (latest {latest_runtime})"),
            );
        }
    }

    if changed {
        ui::status_ok("updated");
    } else {
        ui::status_ok("up to date");
    }
}

/// Normalize a checksum line (bare hex or `<hex>  <file>`) to lowercase hex.
fn normalize_sha(s: String) -> String {
    s.split_whitespace()
        .next()
        .unwrap_or(&s)
        .trim()
        .to_ascii_lowercase()
}

/// Fetch, verify, and atomically install one `.so` from the base into `target`.
fn install_file(
    base: &str,
    name: &str,
    target_dir: &Path,
    expected_override: Option<String>,
    insecure: bool,
) -> Result<(), String> {
    if !valid_asset_name(name) {
        return Err(format!("invalid asset name '{name}'"));
    }
    let expected: Option<String> = match expected_override {
        Some(h) => Some(h),
        None => match fetch_sidecar(base, name)
            .map_err(|e| format!("failed to fetch {name}.sha256 from {base}: {e}"))?
        {
            Some(h) => Some(h),
            None if insecure => None,
            None => {
                return Err(format!(
                    "no checksum available for {name}\n  provide --sha256 <hex>, publish a \
                     {name}.sha256 sidecar, or pass --insecure to skip verification"
                ))
            }
        },
    };
    let expected = expected.map(normalize_sha);

    let target = target_dir.join(name);
    let tmp = reserve_temp(target_dir, name)?;
    if let Err(e) = download_file(base, name, &tmp) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("failed to fetch {name} from {base}: {e}"));
    }

    let actual = match sha256_file(&tmp) {
        Ok(a) => a,
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
    };
    if let Some(exp) = expected {
        if exp != actual {
            let _ = fs::remove_file(&tmp);
            return Err(format!(
                "checksum mismatch for {name}\n  expected {exp}\n  actual   {actual}"
            ));
        }
        ui::ok("checksum", format!("ok ({})", &actual[..12]));
    } else {
        ui::warn_row("checksum", format!("skipped (--insecure)  sha256={actual}"));
    }

    install_from_path(&tmp, &target)?;
    let size = fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
    ui::ok(
        "installed",
        format!("{} ({})", target.display(), crate::ui::human_size(size)),
    );
    Ok(())
}

// ---- pinned -----------------------------------------------------------------

fn update_pinned(
    base: &str,
    version_str: &str,
    sha256: Option<String>,
    insecure: bool,
    home: Option<&str>,
    components: &Components,
    toolchain: ToolchainMode,
) {
    maybe_update_toolchain(base, insecure, toolchain);
    if toolchain == ToolchainMode::Only {
        return;
    }

    let ver = parse_version(version_str)
        .unwrap_or_else(|| fail(&format!("'{version_str}' is not a valid x.y.z version")));
    let target = target_dir(home);
    ensure_dir(&target);

    if components.runtime {
        let name = format!("{FILENAME_PREFIX}{ver}{FILENAME_SUFFIX}");
        ui::section("Runtimes");
        ui::doing("runtime", format!("installing {ver} from {base}"));
        install_file(base, &name, &target, sha256, insecure).unwrap_or_else(|e| fail(&e));
        ui::status_ok(format!("installed inka_runtime {ver}"));
    }
}

/// Run a child process to completion, mapping a spawn/exit failure to a message.
fn run_ok(cmd: &mut Command, what: &str) -> Result<(), String> {
    let status = cmd
        .status()
        .map_err(|e| format!("failed to spawn {what}: {e}"))?;
    if !status.success() {
        return Err(format!("{what} exited with {status}"));
    }
    Ok(())
}

/// Reserve an exclusive temp path next to `target` (so an install is an atomic
/// rename) without ever following a pre-planted symlink.
fn reserve_temp(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let tmp = dir.join(format!(
        ".{name}.tmp{}-{}",
        std::process::id(),
        random_suffix()
    ));
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
    Ok(tmp)
}

/// chmod 0755 and rename a fully-written temp file into place.
fn install_from_path(tmp: &Path, target: &Path) -> Result<(), String> {
    if let Err(e) = fs::set_permissions(tmp, fs::Permissions::from_mode(0o755)) {
        let _ = fs::remove_file(tmp);
        return Err(format!("cannot chmod {}: {e}", tmp.display()));
    }
    if let Err(e) = fs::rename(tmp, target) {
        let _ = fs::remove_file(tmp);
        return Err(format!("cannot move {} into place: {e}", target.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_installs_when_nothing_installed() {
        let a = plan_actions(None, Version(0, 266, 1));
        assert_eq!(a, Actions { runtime: true });
    }

    #[test]
    fn plan_skips_when_current_or_newer() {
        let a = plan_actions(Some(Version(0, 266, 1)), Version(0, 266, 1));
        assert_eq!(a, Actions { runtime: false });

        // Never downgrade: installed newer than the release.
        let b = plan_actions(Some(Version(0, 270, 0)), Version(0, 266, 1));
        assert_eq!(b, Actions { runtime: false });
    }

    #[test]
    fn plan_installs_when_stale() {
        // The base tuple (0.266.0) is behind an inka revision (0.266.1).
        let a = plan_actions(Some(Version(0, 266, 0)), Version(0, 266, 1));
        assert!(a.runtime);
    }

    #[test]
    fn valid_asset_name_cases() {
        for ok in [
            "libinka_runtime-0.266.2.so",
            "inka-toolchain-0.5.3-x86_64-unknown-linux-gnu.tar.gz",
        ] {
            assert!(valid_asset_name(ok), "{ok} should be valid");
        }
        for bad in ["", ".", "..", "../evil", "/abs", "a/b", "a\\b", "dir/../x"] {
            assert!(!valid_asset_name(bad), "{bad} should be invalid");
        }
    }

    #[test]
    fn install_from_path_writes_and_cleans_temp() {
        let dir = std::env::temp_dir().join(format!(
            "inka-install-{}-{}",
            std::process::id(),
            random_suffix()
        ));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("libinka_runtime-0.0.0.so");
        let tmp = reserve_temp(&dir, "libinka_runtime-0.0.0.so").unwrap();
        fs::write(&tmp, b"ELF").unwrap();
        install_from_path(&tmp, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"ELF");
        assert!(!tmp.exists(), "temp must be renamed away");
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp leftovers: {leftovers:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn temp_dir_create_is_exclusive_and_cleans_up() {
        let p;
        {
            let t = TempDir::create("inka-test-").unwrap();
            p = t.0.clone();
            assert!(p.is_dir());
        }
        assert!(!p.exists(), "TempDir should clean up on drop");
    }
}

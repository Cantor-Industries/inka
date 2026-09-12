// inka update: reconcile the toolchain, the shared runtime tuple, and the
// package store with the newest published release (or install an explicit
// runtime tuple).
//
//   inka update [<version>] [--from <dir-or-url>] [--sha256 <hex>]
//                           [--insecure] [--home <dir>]
//                           [--no-toolchain | --toolchain-only]
//                           [--no-runtime] [--no-store]
//
// No <version>: fetch <base>/versions.json, self-update the toolchain when an
// installer-managed one is present, install only the components that are behind
// the newest installed runtime, then sync the store snapshot.
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
use sha2::{Digest, Sha256};

use crate::{
    fetch_text, fetch_with_sidecar, hex, parse_version, Version, FILENAME_PREFIX, FILENAME_SUFFIX,
};

pub(crate) const DEFAULT_CHANNEL: &str =
    "https://github.com/Cantor-Industries/inka/releases/latest/download";

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1);
}

/// Removes a temp directory on drop, so failures don't leave junk behind.
struct TempDir(PathBuf);
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
        eprintln!("error: cannot create {}: {e}", dir.display());
        std::process::exit(1);
    });
}

/// Which components to reconcile. Defaults to all.
#[derive(Clone, Copy)]
struct Components {
    runtime: bool,
    store: bool,
}

impl Default for Components {
    fn default() -> Self {
        Self {
            runtime: true,
            store: true,
        }
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
            "--store-only" => {
                toolchain = ToolchainMode::Skip;
                components.runtime = false;
                components.store = true;
            }
            "--no-runtime" => components.runtime = false,
            "--no-store" => components.store = false,
            "--help" | "-h" => {
                eprintln!(
                    "usage: inka update [<version>] [--from <dir-or-url>] [--sha256 <hex>]\n\
                     \x20                  [--insecure] [--home <dir>]\n\
                     \x20                  [--no-toolchain | --toolchain-only | --store-only]\n\
                     \x20                  [--no-runtime] [--no-store]\n\
                     \x20 no <version>: update the toolchain (if installer-managed) and install the\n\
                     \x20                newest runtime/store that are behind\n\
                     \x20 <version>:     install that exact runtime tuple"
                );
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

    if let (Some(i), Some(l)) = (parse_version(&installed), parse_version(latest)) {
        if i >= l {
            println!("[inka] toolchain {installed} is current (latest {latest})");
            return Ok(false);
        }
    } else if installed == latest {
        println!("[inka] toolchain {installed} is current (latest {latest})");
        return Ok(false);
    }

    println!("[inka] updating toolchain {installed} -> {latest}");
    let (bytes, sidecar) =
        fetch_with_sidecar(base, archive).map_err(|e| format!("failed to fetch {archive}: {e}"))?;
    let expected = tc
        .get("sha256")
        .and_then(Value::as_str)
        .map(|s| s.to_ascii_lowercase())
        .or_else(|| {
            sidecar.map(|s| {
                s.split_whitespace()
                    .next()
                    .unwrap_or(&s)
                    .trim()
                    .to_ascii_lowercase()
                    .to_string()
            })
        });
    let actual = hex(&Sha256::digest(&bytes));
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

    let tmp = TempDir(env::temp_dir().join(format!("inka-toolchain-{}", std::process::id())));
    let _ = fs::remove_dir_all(&tmp.0);
    fs::create_dir_all(&tmp.0).map_err(|e| format!("cannot create {}: {e}", tmp.0.display()))?;
    let archive_path = tmp.0.join(archive);
    fs::write(&archive_path, &bytes)
        .map_err(|e| format!("cannot write {}: {e}", archive_path.display()))?;
    let mut cmd = Command::new("tar");
    cmd.args(["-xzf"]).arg(&archive_path).arg("-C").arg(&tmp.0);
    crate::pkg::run_ok(&mut cmd, "tar extract")?;
    replace_toolchain(&tmp.0, &dir)?;
    fs::write(dir.join("VERSION"), format!("{latest}\n"))
        .map_err(|e| format!("cannot write {}: {e}", dir.join("VERSION").display()))?;
    println!("[inka] installed toolchain {latest}");
    Ok(true)
}

/// Replace the toolchain binaries in `dir` from an extracted archive in
/// `staging`. Binary replacement is an atomic rename over the running image
/// (Linux keeps the old inode until this process exits).
fn replace_toolchain(staging: &Path, dir: &Path) -> Result<(), String> {
    let pid = std::process::id();
    // Stage every binary first: if one is missing or unwritable, nothing is
    // replaced yet and the previous toolchain stays usable.
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
    for f in ["inka", "inka-launcher"] {
        let src = staging.join(f);
        if !src.is_file() {
            cleanup_staged(&staged);
            return Err(format!("toolchain archive is missing '{f}'"));
        }
        let new = dir.join(format!(".{f}.new{pid}"));
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
    eprintln!("[inka] warning: toolchain not updated: {e}");
    eprintln!("[inka]   the engine update continues; re-run install.sh to update the toolchain");
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
        if actions.runtime {
            let name = format!("{FILENAME_PREFIX}{latest_runtime}{FILENAME_SUFFIX}");
            install_file(
                base,
                &name,
                &target,
                runtime_sha.map(str::to_string),
                insecure,
                "inka_runtime",
            )
            .unwrap_or_else(|e| fail(&e));
            changed = true;
        } else if let Some(i) = installed_runtime {
            println!("[inka] runtime {i} is current (latest {latest_runtime})");
        }
    }

    if components.store {
        changed |= sync_store(base);
    }

    if !changed {
        println!("[inka] up to date");
    }
}

/// Fetch, verify, and atomically install one `.so` from the base into `target`.
fn install_file(
    base: &str,
    name: &str,
    target_dir: &Path,
    expected_override: Option<String>,
    insecure: bool,
    label: &str,
) -> Result<(), String> {
    let (bytes, sidecar_sha) = fetch_with_sidecar(base, name)
        .map_err(|e| format!("failed to fetch {name} from {base}: {e}"))?;

    let expected: Option<String> = match (expected_override, sidecar_sha) {
        (Some(h), _) => Some(h),
        (None, Some(h)) => Some(h),
        (None, None) if insecure => None,
        (None, None) => {
            return Err(format!(
                "no checksum available for {name}\n  provide --sha256 <hex>, publish a \
                 {name}.sha256 sidecar, or pass --insecure to skip verification"
            ))
        }
    };
    let expected = expected.map(|e| {
        e.split_whitespace()
            .next()
            .unwrap_or(&e)
            .trim()
            .to_ascii_lowercase()
    });

    let actual = hex(&Sha256::digest(&bytes));
    if let Some(exp) = expected {
        if exp != actual {
            return Err(format!(
                "checksum mismatch for {name}\n  expected {exp}\n  actual   {actual}"
            ));
        }
        println!("[inka] checksum ok ({})", &actual[..12]);
    } else {
        println!("[inka] checksum skipped (--insecure)  sha256={actual}");
    }

    let target = target_dir.join(name);
    install_atomically(&target, &bytes)?;
    println!(
        "[inka] installed {label} {} ({})",
        target.display(),
        bytes.len()
    );
    Ok(())
}

/// Best-effort store sync: fetch the release's store record; if its snapshot
/// identity differs from the local store, replace `node_modules` and record.
/// Returns whether the store changed.
fn sync_store(base: &str) -> bool {
    let store = match env::var_os("INKA_STORE") {
        Some(s) => PathBuf::from(s),
        None => crate::default_store_dir(),
    };
    let record = match crate::pkg::fetch_store_record(base) {
        Ok(r) => r,
        Err(_) => {
            println!("[inka] no package store snapshot at {base}; skipping store");
            return false;
        }
    };
    let remote = crate::pkg::record_sha(&record);
    let local = crate::pkg::store_record_sha(&store);
    if !remote.is_empty() && remote == local {
        println!("[inka] store is current");
        return false;
    }
    let tbytes = match crate::pkg::fetch_store_tar(base, &record) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[inka] warning: store snapshot not applied: {e}");
            return false;
        }
    };
    match crate::pkg::apply_store_record(&store, &record, &tbytes) {
        Ok(n) => {
            println!(
                "[inka] store updated ({n} package(s)) into {}",
                store.display()
            );
            true
        }
        Err(e) => {
            eprintln!("[inka] warning: store snapshot not applied: {e}");
            false
        }
    }
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
        println!("[inka] installing inka_runtime {ver} from {base}");
        install_file(base, &name, &target, sha256, insecure, "inka_runtime")
            .unwrap_or_else(|e| fail(&e));
    }

    // Seed the store from the release snapshot (flat assets or a `store/` subdir).
    if components.store {
        sync_store(base);
    }
}

fn install_atomically(target: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = target.with_extension(format!("so.tmp{}", std::process::id()));
    if let Err(e) = fs::write(&tmp, bytes) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("cannot write {}: {e}", tmp.display()));
    }
    if let Err(e) = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("cannot chmod {}: {e}", tmp.display()));
    }
    fs::rename(&tmp, target).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("cannot move {} into place: {e}", target.display())
    })
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
        let a = plan_actions(Some(Version(0, 266, 0)), Version(0, 266, 1));
        assert_eq!(a, Actions { runtime: true });
    }

    #[test]
    fn runtime_tuple_revision_is_newer_than_base() {
        // The base tuple (0.266.0) is behind an inka revision (0.266.1).
        let a = plan_actions(Some(Version(0, 266, 0)), Version(0, 266, 1));
        assert!(a.runtime);
    }
}

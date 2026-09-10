// inka update: reconcile the shared runtime tuple, resolver, and package store
// with the newest published release (or install an explicit tuple).
//
//   inka update [<version>] [--from <dir-or-url>] [--sha256 <hex>]
//                           [--insecure] [--home <dir>]
//
// No <version>: fetch <base>/versions.json and install only the components that
// are behind the newest installed runtime/resolver, then sync the store
// snapshot. With <version>: install that exact runtime tuple (pinned/offline;
// CI, containers).
//
// The base defaults to the GitHub "latest release" asset base; override with
// --from, $INKA_RELEASE_BASE, or $INKA_RT_SOURCE.

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    fetch_text, fetch_with_sidecar, hex, parse_version, Version, DEFAULT_RESOLVER_VERSION,
    FILENAME_PREFIX, FILENAME_SUFFIX, RESOLVER_PREFIX, RESOLVER_SUFFIX,
};

pub(crate) const DEFAULT_CHANNEL: &str =
    "https://github.com/Cantor-Industries/inka/releases/latest/download";

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1);
}

fn resolve_base(from: Option<String>) -> String {
    from
        .or_else(|| env::var("INKA_RELEASE_BASE").ok())
        .or_else(|| env::var("INKA_RT_SOURCE").ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_CHANNEL.to_string())
}

/// Where this update writes: `--home`, else `INKA_RUNTIME_HOME`, else the
/// system dir when root, else the per-user XDG runtime dir.
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

pub(crate) fn cmd_update(args: &[String]) {
    let mut version = None;
    let mut from = None;
    let mut sha256 = None;
    let mut insecure = false;
    let mut home = None;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--from" => from = it.next().cloned(),
            "--sha256" => sha256 = it.next().cloned(),
            "--home" => home = it.next().cloned(),
            "--insecure" => insecure = true,
            "--help" | "-h" => {
                eprintln!(
                    "usage: inka update [<version>] [--from <dir-or-url>] [--sha256 <hex>]\n\
                     \x20                  [--insecure] [--home <dir>]\n\
                     \x20 no <version>: install the newest runtime/resolver/store from the channel\n\
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
        Some(v) => update_pinned(&base, &v, sha256, insecure, home.as_deref()),
        None => update_latest(&base, insecure, home.as_deref()),
    }
}

// ---- latest -----------------------------------------------------------------

#[derive(Debug, PartialEq)]
struct Actions {
    runtime: bool,
    resolver: bool,
}

/// Decide which components are behind the latest release. A component is only
/// ever fetched when the installed version is strictly older (never downgrade).
fn plan_actions(
    installed_runtime: Option<Version>,
    installed_resolver: Option<Version>,
    latest_runtime: Version,
    latest_resolver: Option<Version>,
) -> Actions {
    let runtime = installed_runtime.map_or(true, |i| i < latest_runtime);
    let resolver = match (latest_resolver, installed_resolver) {
        (Some(l), Some(i)) => i < l,
        (Some(_), None) => true,
        (None, _) => false,
    };
    Actions { runtime, resolver }
}

fn update_latest(base: &str, insecure: bool, home: Option<&str>) {
    let target = target_dir(home);
    let versions_text = match fetch_text(base, "versions.json") {
        Ok(s) => s,
        Err(e) => fail(&format!("cannot read versions.json from {base}: {e}")),
    };
    let v: Value = serde_json::from_str(&versions_text)
        .unwrap_or_else(|e| fail(&format!("invalid versions.json from {base}: {e}")));
    let latest_runtime_s = v
        .get("deno_runtime")
        .and_then(Value::as_str)
        .unwrap_or_else(|| fail("versions.json has no deno_runtime"));
    let latest_runtime = parse_version(latest_runtime_s)
        .unwrap_or_else(|| fail(&format!("invalid deno_runtime '{latest_runtime_s}'")));
    let latest_resolver = v
        .get("resolver")
        .and_then(Value::as_str)
        .and_then(parse_version);
    let runtime_sha = v.get("runtime_sha256").and_then(Value::as_str);

    let search = match home {
        Some(_) => vec![target.clone()],
        None => crate::runtime_search_dirs(),
    };
    let (runtimes, resolvers) = crate::installed_parts_all(&search);
    let installed_runtime = runtimes.last().map(|(ver, _)| *ver);
    let installed_resolver = resolvers.last().map(|(ver, _)| *ver);

    let actions = plan_actions(
        installed_runtime,
        installed_resolver,
        latest_runtime,
        latest_resolver,
    );

    ensure_dir(&target);
    if actions.runtime {
        let name = format!("{FILENAME_PREFIX}{latest_runtime}{FILENAME_SUFFIX}");
        install_file(
            base,
            &name,
            &target,
            runtime_sha.map(str::to_string),
            insecure,
            "inka_runtime",
        );
    } else if let Some(i) = installed_runtime {
        println!("[inka] runtime {i} is current (latest {latest_runtime})");
    }

    if actions.resolver {
        if let Some(res) = latest_resolver {
            let name = format!("{RESOLVER_PREFIX}{res}{RESOLVER_SUFFIX}");
            install_file(base, &name, &target, None, insecure, "resolver");
        }
    } else if let Some(i) = installed_resolver {
        println!("[inka] resolver {i} is current");
    }

    sync_store(base);

    if !actions.runtime && !actions.resolver {
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
) {
    let (bytes, sidecar_sha) = match fetch_with_sidecar(base, name) {
        Ok(x) => x,
        Err(e) => fail(&format!("failed to fetch {name} from {base}: {e}")),
    };

    let expected: Option<String> = match (expected_override, sidecar_sha) {
        (Some(h), _) => Some(h),
        (None, Some(h)) => Some(h),
        (None, None) if insecure => None,
        (None, None) => fail(&format!(
            "no checksum available for {name}\n  provide --sha256 <hex>, publish a {name}.sha256 \
             sidecar, or pass --insecure to skip verification"
        )),
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
            fail(&format!(
                "checksum mismatch for {name}\n  expected {exp}\n  actual   {actual}"
            ));
        }
        println!("[inka] checksum ok ({})", &actual[..12]);
    } else {
        println!("[inka] checksum skipped (--insecure)  sha256={actual}");
    }

    let target = target_dir.join(name);
    install_atomically(&target, &bytes);
    println!(
        "[inka] installed {label} {} ({})",
        target.display(),
        bytes.len()
    );
}

/// Best-effort store sync: fetch the release's store record; if its snapshot
/// identity differs from the local store, replace `node_modules` and record.
fn sync_store(base: &str) {
    let store = match env::var_os("INKA_STORE") {
        Some(s) => PathBuf::from(s),
        None => crate::default_store_dir(),
    };
    let record = match crate::pkg::fetch_store_record(base) {
        Ok(r) => r,
        Err(_) => return, // base ships no store snapshot
    };
    let remote = crate::pkg::record_sha(&record);
    let local = crate::pkg::store_record_sha(&store);
    if !remote.is_empty() && remote == local {
        println!("[inka] store is current");
        return;
    }
    let tbytes = match crate::pkg::fetch_store_tar(base, &record) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[inka] warning: store snapshot not applied: {e}");
            return;
        }
    };
    match crate::pkg::apply_store_record(&store, &record, &tbytes) {
        Ok(n) => println!(
            "[inka] store updated ({n} package(s)) into {}",
            store.display()
        ),
        Err(e) => eprintln!("[inka] warning: store snapshot not applied: {e}"),
    }
}

// ---- pinned -----------------------------------------------------------------

fn update_pinned(
    base: &str,
    version_str: &str,
    sha256: Option<String>,
    insecure: bool,
    home: Option<&str>,
) {
    let ver = parse_version(version_str).unwrap_or_else(|| {
        fail(&format!("'{version_str}' is not a valid x.y.z version"))
    });
    let target = target_dir(home);
    ensure_dir(&target);

    let name = format!("{FILENAME_PREFIX}{ver}{FILENAME_SUFFIX}");
    println!("[inka] installing inka_runtime {ver} from {base}");
    install_file(base, &name, &target, sha256, insecure, "inka_runtime");

    // Ship the runtime release's vendored store payload (if any) into the store.
    let store_target = match env::var_os("INKA_STORE") {
        Some(s) => PathBuf::from(s),
        None => crate::default_store_dir(),
    };
    match crate::pkg::seed_release_store(base, &store_target) {
        Ok(Some(n)) if n > 0 => {
            println!(
                "[inka] installed {n} store package(s) into {}",
                store_target.display()
            );
        }
        Ok(_) => {}
        Err(e) => fail(&format!("store payload: {e}")),
    }

    install_resolver_payload(base, &target, insecure);
}

fn install_resolver_payload(base: &str, target_dir: &Path, insecure: bool) {
    // Pick a resolver from the release: newest libinka_resolver-*.so in a local
    // dir, else a URL fetch of the current resolver version ($INKA_RESOLVER_VERSION
    // overrides; DEFAULT_RESOLVER_VERSION fallback).
    let name = if Path::new(base).is_dir() {
        let mut best: Option<(Version, String)> = None;
        if let Ok(rd) = fs::read_dir(base) {
            for ent in rd.flatten() {
                let n = ent.file_name().to_string_lossy().into_owned();
                let Some(stripped) = n.strip_prefix(RESOLVER_PREFIX) else { continue };
                let Some(vstr) = stripped.strip_suffix(RESOLVER_SUFFIX) else { continue };
                if let Some(v) = parse_version(vstr) {
                    if best.as_ref().map_or(true, |(bv, _)| v > *bv) {
                        best = Some((v, n));
                    }
                }
            }
        }
        best.map(|(_, n)| n)
    } else {
        let ver = env::var("INKA_RESOLVER_VERSION")
            .unwrap_or_else(|_| DEFAULT_RESOLVER_VERSION.to_string());
        Some(format!("{RESOLVER_PREFIX}{ver}{RESOLVER_SUFFIX}"))
    };
    let Some(name) = name else {
        return; // release ships no resolver
    };

    let (bytes, sidecar_sha) = match fetch_with_sidecar(base, &name) {
        Ok(x) => x,
        Err(_) => return, // not present on this source
    };
    let expected = sidecar_sha.and_then(|s| {
        s.split_whitespace()
            .next()
            .map(|x| x.trim().to_ascii_lowercase())
    });
    let actual = hex(&Sha256::digest(&bytes));
    match (&expected, insecure) {
        (Some(exp), _) if exp != &actual => {
            eprintln!("error: checksum mismatch for {name}");
            eprintln!("  expected {exp}");
            eprintln!("  actual   {actual}");
            std::process::exit(1);
        }
        (Some(_), _) => {}
        (None, false) => {
            eprintln!("error: no checksum available for {name}");
            eprintln!("  publish a {name}.sha256 sidecar, or pass --insecure to trust it");
            std::process::exit(1);
        }
        (None, true) => {}
    }
    let target = target_dir.join(&name);
    install_atomically(&target, &bytes);
    println!(
        "[inka] installed resolver {} ({})",
        target.display(),
        bytes.len()
    );
}

fn install_atomically(target: &Path, bytes: &[u8]) {
    let tmp = target.with_extension(format!("so.tmp{}", std::process::id()));
    fs::write(&tmp, bytes).unwrap_or_else(|e| {
        eprintln!("error: cannot write {}: {e}", tmp.display());
        std::process::exit(1);
    });
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap_or_else(|e| {
        eprintln!("error: cannot chmod {}: {e}", tmp.display());
        let _ = fs::remove_file(&tmp);
        std::process::exit(1);
    });
    fs::rename(&tmp, target).unwrap_or_else(|e| {
        eprintln!("error: cannot move {} into place: {e}", target.display());
        let _ = fs::remove_file(&tmp);
        std::process::exit(1);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_installs_when_nothing_installed() {
        let a = plan_actions(None, None, Version(0, 266, 0), Some(Version(1, 0, 0)));
        assert_eq!(a, Actions { runtime: true, resolver: true });
    }

    #[test]
    fn plan_skips_when_current_or_newer() {
        let a = plan_actions(
            Some(Version(0, 266, 0)),
            Some(Version(1, 0, 0)),
            Version(0, 266, 0),
            Some(Version(1, 0, 0)),
        );
        assert_eq!(a, Actions { runtime: false, resolver: false });

        // Never downgrade: installed newer than the release.
        let b = plan_actions(
            Some(Version(0, 270, 0)),
            Some(Version(2, 0, 0)),
            Version(0, 266, 0),
            Some(Version(1, 0, 0)),
        );
        assert_eq!(b, Actions { runtime: false, resolver: false });
    }

    #[test]
    fn plan_installs_only_the_stale_component() {
        let a = plan_actions(
            Some(Version(0, 265, 0)),
            Some(Version(1, 0, 0)),
            Version(0, 266, 0),
            Some(Version(1, 0, 0)),
        );
        assert_eq!(a, Actions { runtime: true, resolver: false });

        let b = plan_actions(
            Some(Version(0, 266, 0)),
            Some(Version(1, 0, 0)),
            Version(0, 266, 0),
            Some(Version(2, 0, 0)),
        );
        assert_eq!(b, Actions { runtime: false, resolver: true });
    }

    #[test]
    fn plan_without_resolver_metadata_never_installs_resolver() {
        let a = plan_actions(Some(Version(0, 266, 0)), None, Version(0, 266, 0), None);
        assert_eq!(a, Actions { runtime: false, resolver: false });
    }
}

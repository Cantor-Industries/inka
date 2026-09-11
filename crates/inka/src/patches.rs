// inka patches: shared discovery, selection, and invocation of repo-managed
// CommonJS->ESM patch specs (`patches/<pkg>/<version>/patch.json`).
//
// A patches base may hold specs for MANY versions of the same package; callers
// select the spec whose exact version matches the installed package. The store
// snapshot patches every occurrence of a package in the resolved node_modules
// tree (hoisted and nested), not just the top-level copy.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// One discovered `patch.json`, with the fields selection needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SpecMeta {
    pub path: PathBuf,
    pub package: String,
    pub version: String,
    pub kind: String,
}

/// One installed package occurrence in a node_modules tree: the package name,
/// its installed version, and the `node_modules` directory that contains it
/// (the value passed to the patcher as `--node-modules`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Occurrence {
    pub node_modules: PathBuf,
    pub package: String,
    pub version: String,
}

/// `base/<package>/<version>/patch.json`.
pub(crate) fn spec_path(base: &Path, package: &str, version: &str) -> PathBuf {
    base.join(package).join(version).join("patch.json")
}

fn load_spec_meta(path: &Path) -> Result<SpecMeta, String> {
    let raw = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let v: Value =
        serde_json::from_slice(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
    let package = v
        .get("package")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{}: missing \"package\"", path.display()))?
        .to_string();
    let version = v
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{}: missing \"version\"", path.display()))?
        .to_string();
    let kind = v
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok(SpecMeta {
        path: path.to_path_buf(),
        package,
        version,
        kind,
    })
}

/// All `patches/<pkg>/<version>/patch.json` specs under a base, parsed and
/// sorted by path. Multiple versions of the same package may coexist.
pub(crate) fn discover_specs(base: &Path) -> Result<Vec<SpecMeta>, String> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(base) else {
        return Ok(out);
    };
    let mut pkg_dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    pkg_dirs.sort();
    for pkg_dir in pkg_dirs {
        let Ok(vers) = fs::read_dir(&pkg_dir) else {
            continue;
        };
        let mut ver_dirs: Vec<PathBuf> = vers
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        ver_dirs.sort();
        for v in ver_dirs {
            let path = v.join("patch.json");
            if path.is_file() {
                out.push(load_spec_meta(&path)?);
            }
        }
    }
    Ok(out)
}

/// The spec for an exact `package@version`, or `None` when no spec exists.
pub(crate) fn spec_for(
    base: &Path,
    package: &str,
    version: &str,
) -> Result<Option<SpecMeta>, String> {
    let path = spec_path(base, package, version);
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(load_spec_meta(&path)?))
}

/// Every package occurrence in a `node_modules` tree, including scoped packages
/// and nested `node_modules` duplicates, in deterministic (sorted, shallow-first)
/// order.
pub(crate) fn package_occurrences(node_modules: &Path) -> Vec<Occurrence> {
    let mut out = Vec::new();
    walk_packages(node_modules, &mut out);
    out
}

fn walk_packages(node_modules: &Path, out: &mut Vec<Occurrence>) {
    let Ok(entries) = fs::read_dir(node_modules) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let name = dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if name.starts_with('.') {
            continue;
        }
        if name.starts_with('@') {
            // scope container: its children are packages
            let Ok(subs) = fs::read_dir(&dir) else {
                continue;
            };
            let mut sub_dirs: Vec<PathBuf> = subs
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            sub_dirs.sort();
            for pkg_dir in sub_dirs {
                let pkg = pkg_dir
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                record_package(&pkg_dir, &format!("{name}/{pkg}"), node_modules, out);
            }
        } else {
            record_package(&dir, &name, node_modules, out);
        }
    }
}

fn record_package(pkg_dir: &Path, package: &str, node_modules: &Path, out: &mut Vec<Occurrence>) {
    let Ok(raw) = fs::read(pkg_dir.join("package.json")) else {
        return;
    };
    let Ok(v) = serde_json::from_slice::<Value>(&raw) else {
        return;
    };
    let Some(version) = v.get("version").and_then(Value::as_str) else {
        return;
    };
    out.push(Occurrence {
        node_modules: node_modules.to_path_buf(),
        package: package.to_string(),
        version: version.to_string(),
    });
    let nested = pkg_dir.join("node_modules");
    if nested.is_dir() {
        walk_packages(&nested, out);
    }
}

/// Plan a store-snapshot patch pass: for every occurrence of a package that has
/// an exact-version spec, pair the spec with the occurrence's containing
/// `node_modules`; return the pairs to apply and the specs that matched nothing.
pub(crate) fn plan_snapshot(
    base: &Path,
    node_modules: &Path,
) -> Result<(Vec<(SpecMeta, PathBuf)>, Vec<SpecMeta>), String> {
    let specs = discover_specs(base)?;
    if specs.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut applies = Vec::new();
    let mut matched: BTreeSet<(String, String)> = BTreeSet::new();
    for occ in package_occurrences(node_modules) {
        if let Some(spec) = specs
            .iter()
            .find(|s| s.package == occ.package && s.version == occ.version)
        {
            applies.push((spec.clone(), occ.node_modules));
            matched.insert((spec.package.clone(), spec.version.clone()));
        }
    }
    let skipped = specs
        .into_iter()
        .filter(|s| !matched.contains(&(s.package.clone(), s.version.clone())))
        .collect();
    Ok((applies, skipped))
}

// ---- patcher invocation ----------------------------------------------------

/// Locate the `inka-patcher` binary: `$INKA_PATCHER`, else next to this binary.
pub(crate) fn patcher_binary() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("INKA_PATCHER") {
        if Path::new(&p).is_file() {
            return Ok(PathBuf::from(p));
        }
        return Err(format!("INKA_PATCHER points to a missing file: {p}"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("inka-patcher");
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    Err(
        "patch specs exist but no inka-patcher binary found; build it with the big-disk \
         cargo home/target (cargo build --release -p inka-patcher) and keep it next to \
         this inka binary (or set INKA_PATCHER)"
            .into(),
    )
}

/// Apply one spec in place to the package under `node_modules`.
pub(crate) fn invoke(spec_path: &Path, node_modules: &Path) -> Result<(), String> {
    let bin = patcher_binary()?;
    let mut cmd = Command::new(&bin);
    cmd.arg("apply")
        .arg("--spec")
        .arg(spec_path)
        .arg("--node-modules")
        .arg(node_modules);
    crate::pkg::run_ok(
        &mut cmd,
        &format!("inka-patcher apply {}", spec_path.display()),
    )
}

// ---- base discovery --------------------------------------------------------

/// Release/snapshot base: `--patches <dir>`, else `<seed-manifest dir>/patches`.
pub(crate) fn release_base(patches_flag: Option<&str>, seed_manifest: &Path) -> PathBuf {
    if let Some(p) = patches_flag {
        return PathBuf::from(p);
    }
    seed_manifest
        .parent()
        .map(|d| d.join("patches"))
        .unwrap_or_else(|| PathBuf::from("patches"))
}

/// Project base: `$INKA_PATCHES` -> `./patches` -> `<dir of inka binary>/patches`.
/// `$INKA_PATCHES` wins even if it does not exist yet.
pub(crate) fn project_base() -> PathBuf {
    match project_base_source() {
        Some(p) => p,
        None => std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("patches"),
    }
}

/// `Some(dir)` when project patch specs are discoverable somewhere inka looks.
pub(crate) fn project_base_discoverable() -> bool {
    project_base_source().is_some()
}

fn project_base_source() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("INKA_PATCHES") {
        return Some(PathBuf::from(p));
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let local = cwd.join("patches");
    if local.is_dir() {
        return Some(local);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("patches");
            if p.is_dir() {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn scratch(kind: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("inkapatches-{kind}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    fn spec_json(pkg: &str, ver: &str) -> String {
        format!(r#"{{"package":"{pkg}","version":"{ver}","type":"bundle-esm"}}"#)
    }

    #[test]
    fn discover_specs_finds_multiple_versions() {
        let base = scratch("discover");
        write(&base, "ws/8.21.3/patch.json", &spec_json("ws", "8.21.3"));
        write(&base, "ws/8.20.0/patch.json", &spec_json("ws", "8.20.0"));
        write(&base, "mime/3.0.0/patch.json", &spec_json("mime", "3.0.0"));
        let specs = discover_specs(&base).unwrap();
        let keys: Vec<(String, String)> = specs
            .iter()
            .map(|s| (s.package.clone(), s.version.clone()))
            .collect();
        assert_eq!(
            keys,
            vec![
                ("mime".to_string(), "3.0.0".to_string()),
                ("ws".to_string(), "8.20.0".to_string()),
                ("ws".to_string(), "8.21.3".to_string()),
            ]
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn spec_for_selects_exact_version_only() {
        let base = scratch("specfor");
        write(&base, "ws/8.21.3/patch.json", &spec_json("ws", "8.21.3"));
        assert!(spec_for(&base, "ws", "8.21.3").unwrap().is_some());
        assert!(spec_for(&base, "ws", "8.20.0").unwrap().is_none());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn package_occurrences_walks_nested_and_scoped() {
        let nm = scratch("occ");
        write(
            &nm,
            "ws/package.json",
            r#"{"name":"ws","version":"8.21.3"}"#,
        );
        write(
            &nm,
            "@scope/pkg/package.json",
            r#"{"name":"@scope/pkg","version":"1.0.0"}"#,
        );
        write(&nm, "a/package.json", r#"{"name":"a","version":"1.0.0"}"#);
        write(
            &nm,
            "a/node_modules/ws/package.json",
            r#"{"name":"ws","version":"8.20.0"}"#,
        );
        write(
            &nm,
            ".bin/ignored/package.json",
            r#"{"name":"ignored","version":"9.9.9"}"#,
        );
        let occ = package_occurrences(&nm);
        let found: Vec<(String, String)> = occ
            .iter()
            .map(|o| (o.package.clone(), o.version.clone()))
            .collect();
        assert!(
            found.contains(&("ws".to_string(), "8.21.3".to_string())),
            "{found:?}"
        );
        assert!(
            found.contains(&("@scope/pkg".to_string(), "1.0.0".to_string())),
            "{found:?}"
        );
        assert!(
            found.contains(&("a".to_string(), "1.0.0".to_string())),
            "{found:?}"
        );
        assert!(
            found.contains(&("ws".to_string(), "8.20.0".to_string())),
            "{found:?}"
        );
        assert!(!found.iter().any(|(n, _)| n == "ignored"), "{found:?}");
        // the nested ws reports its own containing node_modules
        let nested = occ
            .iter()
            .find(|o| o.package == "ws" && o.version == "8.20.0")
            .unwrap();
        assert_eq!(nested.node_modules, nm.join("a").join("node_modules"));
        let _ = fs::remove_dir_all(&nm);
    }

    #[test]
    fn plan_snapshot_pairs_matches_and_reports_skips() {
        let base = scratch("plan-base");
        let nm = scratch("plan-nm");
        write(&base, "ws/8.21.3/patch.json", &spec_json("ws", "8.21.3"));
        write(&base, "ws/8.20.0/patch.json", &spec_json("ws", "8.20.0"));
        write(&base, "mime/3.0.0/patch.json", &spec_json("mime", "3.0.0"));
        write(
            &nm,
            "ws/package.json",
            r#"{"name":"ws","version":"8.21.3"}"#,
        );
        write(&nm, "a/package.json", r#"{"name":"a","version":"1.0.0"}"#);
        write(
            &nm,
            "a/node_modules/ws/package.json",
            r#"{"name":"ws","version":"8.20.0"}"#,
        );
        let (applies, skipped) = plan_snapshot(&base, &nm).unwrap();
        let applied: Vec<(String, String)> = applies
            .iter()
            .map(|(s, _)| (s.package.clone(), s.version.clone()))
            .collect();
        assert!(
            applied.contains(&("ws".to_string(), "8.21.3".to_string())),
            "{applied:?}"
        );
        assert!(
            applied.contains(&("ws".to_string(), "8.20.0".to_string())),
            "{applied:?}"
        );
        assert_eq!(applies.len(), 2);
        let skipped_keys: Vec<(String, String)> = skipped
            .iter()
            .map(|s| (s.package.clone(), s.version.clone()))
            .collect();
        assert_eq!(
            skipped_keys,
            vec![("mime".to_string(), "3.0.0".to_string())]
        );
        let _ = fs::remove_dir_all(&base);
        let _ = fs::remove_dir_all(&nm);
    }
}

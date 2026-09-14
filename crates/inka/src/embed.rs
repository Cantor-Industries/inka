// inka build embed helpers: extra files to carry inside an artifact.
//
// The entry graph itself is bundled by `inka-bundler`. These helpers collect
// the remaining files: `--embed-dir` (the whole cwd tree) and `--external`
// package trees from `node_modules`.
//
// Entries are returned as `(path-relative-to-cwd, bytes)` pairs.

#[cfg(feature = "bundle")]
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;
#[cfg(feature = "bundle")]
use std::path::PathBuf;

#[cfg(feature = "bundle")]
const IGNORE_DIRS: [&str; 5] = [".git", "target", "node_modules", ".inka", "dist"];

/// Cwd-relative path (with `/` separators) for a file under the cwd.
pub fn rel_from_cwd(cwd: &Path, p: &Path) -> Result<String, String> {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    let canon =
        fs::canonicalize(&abs).map_err(|e| format!("cannot resolve {}: {e}", abs.display()))?;
    let canon_cwd = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let rel = canon.strip_prefix(&canon_cwd).map_err(|_| {
        format!(
            "source '{}' is outside the current working directory; run inka build from the project root",
            abs.display()
        )
    })?;
    Ok(normalize_rel(rel))
}

fn normalize_rel(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Embed the whole cwd tree minus ignore dirs (`--embed-dir`). The entry is
/// returned first for deterministic ordering.
#[cfg(feature = "bundle")]
pub fn collect_directory(cwd: &Path, entry_rel: &str) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    walk(cwd, cwd, &mut files)?;
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    if let Some(b) = files.remove(entry_rel) {
        out.push((entry_rel.to_string(), b));
    }
    for (k, v) in files {
        out.push((k, v));
    }
    Ok(out)
}

/// Embed an external package (`--external`) together with its **transitive
/// runtime dependency closure** at `node_modules/**`, so the artifact resolves
/// it without the project `node_modules`.
///
/// The closure is walked from each package's canonical realpath and every dep
/// is resolved with a Node-style nearest-`node_modules` lookup, so hoisted
/// (npm/yarn/bun) and symlinked isolated (pnpm `.pnpm/`, yarn, bun) layouts
/// both work. Deps are flattened to `node_modules/<name>`; a name that resolves
/// to a second version is nested under the referring package
/// (`node_modules/<pkg>/node_modules/<name>`) so nearest-wins still holds.
#[cfg(feature = "bundle")]
pub fn collect_package(cwd: &Path, pkg: &str) -> Result<Vec<(String, Vec<u8>)>, String> {
    let project_nm = cwd.join("node_modules");
    let root = project_nm.join(pkg);
    if !root.is_dir() {
        return Err(format!(
            "external package '{pkg}' not found at {}",
            root.display()
        ));
    }
    let project_nm = fs::canonicalize(&project_nm).unwrap_or(project_nm);
    let root_real = fs::canonicalize(&root).unwrap_or(root);

    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut name_to_folder: HashMap<String, PathBuf> = HashMap::new();
    let mut visited: HashSet<(PathBuf, String)> = HashSet::new();
    let mut queue: VecDeque<(PathBuf, String)> = VecDeque::new();

    name_to_folder.insert(pkg.to_string(), root_real.clone());
    queue.push_back((root_real, format!("node_modules/{pkg}")));

    while let Some((folder, prefix)) = queue.pop_front() {
        if !visited.insert((folder.clone(), prefix.clone())) {
            continue;
        }
        walk_package(&folder, Path::new(&prefix), &mut files)?;
        for dep in package_dep_names(&folder) {
            let Some(dep_folder) = resolve_dep(&project_nm, &folder, &dep) else {
                continue;
            };
            let dep_real = fs::canonicalize(&dep_folder).unwrap_or(dep_folder);
            let dep_prefix = match name_to_folder.get(&dep) {
                Some(existing) if *existing == dep_real => continue, // same version, already queued
                Some(_) => format!("{prefix}/node_modules/{dep}"),   // conflict: nest
                None => {
                    name_to_folder.insert(dep.clone(), dep_real.clone());
                    format!("node_modules/{dep}")
                }
            };
            queue.push_back((dep_real, dep_prefix));
        }
    }

    Ok(files.into_iter().collect())
}

/// Runtime dependency names declared by a package (`dependencies`,
/// `optionalDependencies`, `peerDependencies`), in a stable order.
#[cfg(feature = "bundle")]
fn package_dep_names(folder: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(folder.join("package.json")) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let mut names: Vec<String> = Vec::new();
    for key in ["dependencies", "optionalDependencies", "peerDependencies"] {
        if let Some(obj) = value.get(key).and_then(|v| v.as_object()) {
            for name in obj.keys() {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
    }
    names
}

/// Node-style nearest-`node_modules/<name>` lookup from `referrer`'s realpath,
/// climbing to the project root (`project_nm`'s parent) and no further.
#[cfg(feature = "bundle")]
fn resolve_dep(project_nm: &Path, referrer: &Path, name: &str) -> Option<PathBuf> {
    let tree = project_nm.parent()?;
    let mut dir = referrer.to_path_buf();
    loop {
        if !dir.starts_with(tree) {
            return None;
        }
        let candidate = dir.join("node_modules").join(name);
        if candidate.join("package.json").is_file() {
            return Some(candidate);
        }
        if dir == tree {
            return None;
        }
        dir = dir.parent()?.to_path_buf();
    }
}

#[cfg(feature = "bundle")]
fn walk(cwd: &Path, dir: &Path, files: &mut BTreeMap<String, Vec<u8>>) -> Result<(), String> {
    let rd = fs::read_dir(dir).map_err(|e| format!("cannot read dir {}: {e}", dir.display()))?;
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = ent.path();
        let ft = ent.file_type().map_err(|e| e.to_string())?;
        if ft.is_dir() {
            if IGNORE_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk(cwd, &path, files)?;
        } else if ft.is_file() {
            if let (Ok(bytes), Ok(rel)) = (fs::read(&path), path.strip_prefix(cwd)) {
                files.insert(normalize_rel(rel), bytes);
            }
        }
    }
    Ok(())
}

/// Walk a package directory, mapping each file under `prefix`. A nested
/// `node_modules` keeps its `node_modules/<pkg>/…` layout. Symlinks are
/// followed (pnpm/yarn isolated stores), with canonical-path cycle detection.
#[cfg(feature = "bundle")]
fn walk_package(
    dir: &Path,
    prefix: &Path,
    files: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), String> {
    walk_package_inner(dir, prefix, files, &mut HashSet::new())
}

#[cfg(feature = "bundle")]
fn walk_package_inner(
    dir: &Path,
    prefix: &Path,
    files: &mut BTreeMap<String, Vec<u8>>,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), String> {
    let canon = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if !visited.insert(canon) {
        return Ok(());
    }
    let rd = fs::read_dir(dir).map_err(|e| format!("cannot read dir {}: {e}", dir.display()))?;
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = ent.path();
        // `metadata` follows symlinks so isolated-store entries are included.
        let md = fs::metadata(&path).map_err(|e| e.to_string())?;
        if md.is_dir() {
            walk_package_inner(&path, &prefix.join(&name), files, visited)?;
        } else if md.is_file() {
            if let Ok(bytes) = fs::read(&path) {
                files.insert(normalize_rel(&prefix.join(&name)), bytes);
            }
        }
    }
    Ok(())
}

#[cfg(all(test, feature = "bundle"))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn scratch() -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("inkaembed-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn mk(cwd: &Path, rel: &str, body: &str) {
        let p = cwd.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn collect_package_includes_nested_node_modules() {
        let cwd = scratch();
        mk(&cwd, "node_modules/pkg/package.json", "{}");
        mk(&cwd, "node_modules/pkg/index.js", "module.exports = 1;\n");
        mk(&cwd, "node_modules/pkg/node_modules/dep/package.json", "{}");
        mk(
            &cwd,
            "node_modules/pkg/node_modules/dep/index.js",
            "module.exports = 2;\n",
        );
        let files = collect_package(&cwd, "pkg").unwrap();
        let rels: Vec<String> = files.iter().map(|(r, _)| r.clone()).collect();
        assert!(
            rels.contains(&"node_modules/pkg/index.js".to_string()),
            "{rels:?}"
        );
        assert!(
            rels.contains(&"node_modules/pkg/node_modules/dep/index.js".to_string()),
            "{rels:?}"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn collect_directory_skips_ignored_dirs() {
        let cwd = scratch();
        mk(&cwd, "app.js", "console.log(1);\n");
        mk(&cwd, "assets/logo.txt", "hi\n");
        mk(&cwd, "node_modules/pkg/index.js", "x\n");
        let files = collect_directory(&cwd, "app.js").unwrap();
        let rels: Vec<String> = files.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(rels[0], "app.js", "{rels:?}");
        assert!(rels.contains(&"assets/logo.txt".to_string()), "{rels:?}");
        assert!(
            !rels.iter().any(|r| r.starts_with("node_modules")),
            "{rels:?}"
        );
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn collect_package_embeds_hoisted_closure() {
        let cwd = scratch();
        mk(
            &cwd,
            "node_modules/dbg/package.json",
            r#"{"name":"dbg","version":"1.0.0","main":"index.js","dependencies":{"ms":"^2.1.3"}}"#,
        );
        mk(&cwd, "node_modules/dbg/index.js", "require(\"ms\");\n");
        mk(
            &cwd,
            "node_modules/ms/package.json",
            r#"{"name":"ms","version":"2.1.3","main":"index.js"}"#,
        );
        mk(&cwd, "node_modules/ms/index.js", "module.exports = 1;\n");
        let files = collect_package(&cwd, "dbg").unwrap();
        let rels: Vec<String> = files.iter().map(|(r, _)| r.clone()).collect();
        assert!(
            rels.contains(&"node_modules/dbg/index.js".to_string()),
            "{rels:?}"
        );
        assert!(
            rels.contains(&"node_modules/ms/index.js".to_string()),
            "{rels:?}"
        );
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn collect_package_follows_pnpm_symlinks() {
        let cwd = scratch();
        mk(
            &cwd,
            "node_modules/.pnpm/a@1.0.0/node_modules/a/package.json",
            r#"{"name":"a","version":"1.0.0","type":"module","main":"index.js","dependencies":{"b":"1.0.0"}}"#,
        );
        mk(
            &cwd,
            "node_modules/.pnpm/a@1.0.0/node_modules/a/index.js",
            "import b from \"b\";\nexport default b;\n",
        );
        mk(
            &cwd,
            "node_modules/.pnpm/b@1.0.0/node_modules/b/package.json",
            r#"{"name":"b","version":"1.0.0","type":"module","main":"index.js"}"#,
        );
        mk(
            &cwd,
            "node_modules/.pnpm/b@1.0.0/node_modules/b/index.js",
            "export default 1;\n",
        );
        // pnpm layout: root symlink to the real folder, and a sibling dep
        // symlink beside a's realpath.
        std::os::unix::fs::symlink(".pnpm/a@1.0.0/node_modules/a", cwd.join("node_modules/a"))
            .unwrap();
        std::os::unix::fs::symlink(
            "../../b@1.0.0/node_modules/b",
            cwd.join("node_modules/.pnpm/a@1.0.0/node_modules/b"),
        )
        .unwrap();

        let files = collect_package(&cwd, "a").unwrap();
        let rels: Vec<String> = files.iter().map(|(r, _)| r.clone()).collect();
        assert!(
            rels.contains(&"node_modules/a/index.js".to_string()),
            "{rels:?}"
        );
        assert!(
            rels.contains(&"node_modules/b/index.js".to_string()),
            "{rels:?}"
        );
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn collect_package_nests_conflicting_versions() {
        let cwd = scratch();
        // root pkg -> dep; two packages depend on different dep versions.
        mk(
            &cwd,
            "node_modules/top/package.json",
            r#"{"name":"top","version":"1.0.0","main":"index.js","dependencies":{"dep":"1.0.0","other":"1.0.0"}}"#,
        );
        mk(&cwd, "node_modules/top/index.js", "module.exports = 0;\n");
        mk(
            &cwd,
            "node_modules/dep/package.json",
            r#"{"name":"dep","version":"1.0.0","main":"index.js"}"#,
        );
        mk(&cwd, "node_modules/dep/index.js", "module.exports = 1;\n");
        mk(
            &cwd,
            "node_modules/other/package.json",
            r#"{"name":"other","version":"1.0.0","main":"index.js","dependencies":{"dep":"2.0.0"}}"#,
        );
        mk(&cwd, "node_modules/other/index.js", "module.exports = 2;\n");
        // `other` has its own nested dep@2.
        mk(
            &cwd,
            "node_modules/other/node_modules/dep/package.json",
            r#"{"name":"dep","version":"2.0.0","main":"index.js"}"#,
        );
        mk(
            &cwd,
            "node_modules/other/node_modules/dep/index.js",
            "module.exports = 2;\n",
        );
        let files = collect_package(&cwd, "top").unwrap();
        let rels: Vec<String> = files.iter().map(|(r, _)| r.clone()).collect();
        assert!(
            rels.contains(&"node_modules/dep/index.js".to_string()),
            "{rels:?}"
        );
        assert!(
            rels.contains(&"node_modules/other/node_modules/dep/index.js".to_string()),
            "{rels:?}"
        );
        let _ = fs::remove_dir_all(&cwd);
    }
}

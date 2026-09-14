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
/// The package is located with a Node-style nearest-`node_modules` lookup from
/// the entry's directory (`entry_dir`), so a workspace member's package
/// (`packages/app/node_modules/@scope/other` → `packages/other`) is found even
/// though it is not hoisted to `<cwd>/node_modules`. The closure is walked from
/// each package's canonical realpath and every dep is resolved the same way, so
/// hoisted (npm/yarn/bun) and symlinked isolated (pnpm `.pnpm/`, yarn, bun)
/// layouts both work. Deps are flattened to `node_modules/<name>`; a name that
/// resolves to a second version is nested under the referring package
/// (`node_modules/<pkg>/node_modules/<name>`) so nearest-wins still holds.
#[cfg(feature = "bundle")]
pub fn collect_package(
    cwd: &Path,
    entry_dir: &Path,
    pkg: &str,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    if !valid_package_name(pkg) {
        return Err(format!("invalid external package name '{pkg}'"));
    }
    // Confine every walked realpath to the project tree (workspace symlinks and
    // isolated stores stay inside it; a malicious dep cannot pull in /etc).
    let tree_real = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let Some(root) = find_package_root(&tree_real, entry_dir, pkg) else {
        return Err(format!(
            "external package '{pkg}' not found in a nearest node_modules under {}",
            cwd.display()
        ));
    };
    let root_real = fs::canonicalize(&root).unwrap_or(root);
    if !root_real.starts_with(&tree_real) {
        return Err(format!(
            "external package '{pkg}' resolves outside the project tree"
        ));
    }

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
        walk_package(&folder, Path::new(&prefix), &mut files, &tree_real)?;
        for dep in package_dep_names(&folder) {
            let Some(dep_folder) = resolve_dep(&tree_real, &folder, &dep) else {
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

/// A valid npm package name: `name` or `@scope/name`, with no path separators,
/// no `.`/`..`, and no backslash. Guards `node_modules/<name>` joins against
/// traversal via an attacker-controlled `dependencies` key or `--external`.
#[cfg(feature = "bundle")]
fn valid_package_name(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." || name.contains('\\') || name.contains('\0')
    {
        return false;
    }
    match name.strip_prefix('@') {
        Some(rest) => match rest.split_once('/') {
            Some((scope, pkg)) => {
                !scope.is_empty()
                    && !pkg.is_empty()
                    && !pkg.contains('/')
                    && scope != "."
                    && scope != ".."
                    && pkg != "."
                    && pkg != ".."
            }
            None => false,
        },
        None => !name.contains('/'),
    }
}

/// Runtime dependency names declared by a package (`dependencies`,
/// `optionalDependencies`, `peerDependencies`), in a stable order. Names that
/// fail `valid_package_name` are skipped.
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
                if valid_package_name(name) && !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
    }
    names
}

/// Locate an external package's folder with a Node-style nearest-`node_modules`
/// lookup from `start`, climbing to the project root (`tree`) and no further.
#[cfg(feature = "bundle")]
fn find_package_root(tree: &Path, start: &Path, name: &str) -> Option<PathBuf> {
    let start = fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    let mut dir = start;
    loop {
        if !dir.starts_with(tree) {
            return None;
        }
        let candidate = dir.join("node_modules").join(name);
        if candidate.is_dir() {
            return Some(candidate);
        }
        if dir == tree {
            return None;
        }
        dir = dir.parent()?.to_path_buf();
    }
}

/// Node-style nearest-`node_modules/<name>` lookup from `referrer`'s realpath,
/// climbing to the project root (`tree`) and no further. A candidate whose
/// realpath escapes the project tree is rejected (symlinked dep escape).
#[cfg(feature = "bundle")]
fn resolve_dep(tree: &Path, referrer: &Path, name: &str) -> Option<PathBuf> {
    let mut dir = referrer.to_path_buf();
    loop {
        if !dir.starts_with(tree) {
            return None;
        }
        let candidate = dir.join("node_modules").join(name);
        if candidate.join("package.json").is_file() {
            let real = fs::canonicalize(&candidate).ok()?;
            return real.starts_with(tree).then_some(real);
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
/// followed (pnpm/yarn isolated stores) only when their realpath stays under
/// `allowed`, with canonical-path cycle detection.
#[cfg(feature = "bundle")]
fn walk_package(
    dir: &Path,
    prefix: &Path,
    files: &mut BTreeMap<String, Vec<u8>>,
    allowed: &Path,
) -> Result<(), String> {
    walk_package_inner(dir, prefix, files, &mut HashSet::new(), allowed)
}

#[cfg(feature = "bundle")]
fn walk_package_inner(
    dir: &Path,
    prefix: &Path,
    files: &mut BTreeMap<String, Vec<u8>>,
    visited: &mut HashSet<PathBuf>,
    allowed: &Path,
) -> Result<(), String> {
    let canon = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if !canon.starts_with(allowed) || !visited.insert(canon) {
        return Ok(());
    }
    let rd = fs::read_dir(dir).map_err(|e| format!("cannot read dir {}: {e}", dir.display()))?;
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = ent.path();
        // `metadata` follows symlinks so isolated-store entries are included;
        // `canonicalize` then keeps them inside the project tree.
        let md = fs::metadata(&path).map_err(|e| e.to_string())?;
        if md.is_dir() {
            if let Ok(real) = fs::canonicalize(&path) {
                if !real.starts_with(allowed) {
                    continue;
                }
            }
            walk_package_inner(&path, &prefix.join(&name), files, visited, allowed)?;
        } else if md.is_file() {
            if let Ok(real) = fs::canonicalize(&path) {
                if !real.starts_with(allowed) {
                    continue;
                }
            }
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
        let files = collect_package(&cwd, &cwd, "pkg").unwrap();
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
    fn valid_package_name_cases() {
        for ok in ["ms", "debug", "@scope/pkg", "@parcel/watcher"] {
            assert!(valid_package_name(ok), "{ok} should be valid");
        }
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "@scope",
            "@scope/",
            "@scope/../x",
            "a\\b",
            "@",
        ] {
            assert!(!valid_package_name(bad), "{bad} should be invalid");
        }
    }

    #[test]
    fn collect_package_rejects_bad_external_name() {
        let cwd = scratch();
        mk(&cwd, "node_modules/pkg/package.json", r#"{"name":"pkg"}"#);
        assert!(collect_package(&cwd, &cwd, "../etc").is_err());
        assert!(collect_package(&cwd, &cwd, "a/b").is_err());
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn collect_package_skips_out_of_tree_symlinks() {
        let cwd = scratch();
        let outside = scratch();
        // An out-of-tree package that a malicious dependency name points at.
        mk(
            &outside,
            "evil/package.json",
            r#"{"name":"evil","main":"index.js"}"#,
        );
        mk(&outside, "evil/index.js", "LEAK\n");
        // A package whose dependency resolves, via symlink, outside the tree.
        mk(
            &cwd,
            "node_modules/pkg/package.json",
            r#"{"name":"pkg","main":"index.js","dependencies":{"evil":"1.0.0"}}"#,
        );
        mk(&cwd, "node_modules/pkg/index.js", "module.exports = 1;\n");
        std::os::unix::fs::symlink(outside.join("evil"), cwd.join("node_modules/evil")).unwrap();
        // A symlinked file inside the package escaping the tree.
        mk(&outside, "secret.js", "SECRET\n");
        std::os::unix::fs::symlink(
            outside.join("secret.js"),
            cwd.join("node_modules/pkg/leak.js"),
        )
        .unwrap();

        let files = collect_package(&cwd, &cwd, "pkg").unwrap();
        let rels: Vec<String> = files.iter().map(|(r, _)| r.clone()).collect();
        assert!(
            rels.contains(&"node_modules/pkg/index.js".to_string()),
            "{rels:?}"
        );
        assert!(
            !rels.iter().any(|r| r.contains("evil")),
            "out-of-tree dep embedded: {rels:?}"
        );
        assert!(
            !rels.contains(&"node_modules/pkg/leak.js".to_string()),
            "out-of-tree file embedded: {rels:?}"
        );
        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&outside);
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
        let files = collect_package(&cwd, &cwd, "dbg").unwrap();
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
    fn collect_package_resolves_workspace_member_from_entry_dir() {
        let cwd = scratch();
        mk(
            &cwd,
            "packages/app/package.json",
            r#"{"name":"@scope/app","dependencies":{"@scope/other":"workspace:*"}}"#,
        );
        mk(
            &cwd,
            "packages/other/package.json",
            r#"{"name":"@scope/other","exports":{".":"./src/index.ts"}}"#,
        );
        mk(&cwd, "packages/other/src/index.ts", "export const x = 1;\n");
        std::fs::create_dir_all(cwd.join("packages/app/node_modules/@scope")).unwrap();
        std::os::unix::fs::symlink(
            "../../../other",
            cwd.join("packages/app/node_modules/@scope/other"),
        )
        .unwrap();
        let entry_dir = cwd.join("packages/app/src");
        std::fs::create_dir_all(&entry_dir).unwrap();

        let files = collect_package(&cwd, &entry_dir, "@scope/other").unwrap();
        let rels: Vec<String> = files.iter().map(|(r, _)| r.clone()).collect();
        assert!(
            rels.contains(&"node_modules/@scope/other/src/index.ts".to_string()),
            "{rels:?}"
        );
        let _ = std::fs::remove_dir_all(&cwd);
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

        let files = collect_package(&cwd, &cwd, "a").unwrap();
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
        let files = collect_package(&cwd, &cwd, "top").unwrap();
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

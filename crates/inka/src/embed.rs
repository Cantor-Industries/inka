// inka build embed helpers: extra files to carry inside an artifact.
//
// The entry graph itself is bundled by `inka-bundler`. These helpers collect
// the remaining files: `--embed-dir` (the whole cwd tree) and `--external`
// package trees from `node_modules`.
//
// Entries are returned as `(path-relative-to-cwd, bytes)` pairs.

#[cfg(feature = "bundle")]
use std::collections::BTreeMap;
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

/// Embed a package tree from `<cwd>/node_modules/<pkg>/**` (nested
/// `node_modules` included) at `node_modules/<pkg>/**` (`--external`).
#[cfg(feature = "bundle")]
pub fn collect_package(cwd: &Path, pkg: &str) -> Result<Vec<(String, Vec<u8>)>, String> {
    let root = cwd.join("node_modules").join(pkg);
    if !root.is_dir() {
        return Err(format!(
            "external package '{pkg}' not found at {}",
            root.display()
        ));
    }
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let prefix = PathBuf::from("node_modules").join(pkg);
    walk_package(&root, &prefix, &mut files)?;
    Ok(files.into_iter().collect())
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
/// `node_modules` keeps its `node_modules/<pkg>/…` layout.
#[cfg(feature = "bundle")]
fn walk_package(
    dir: &Path,
    prefix: &Path,
    files: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), String> {
    let rd = fs::read_dir(dir).map_err(|e| format!("cannot read dir {}: {e}", dir.display()))?;
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = ent.path();
        let ft = ent.file_type().map_err(|e| e.to_string())?;
        if ft.is_dir() {
            walk_package(&path, &prefix.join(&name), files)?;
        } else if ft.is_file() {
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
        let _ = std::fs::remove_dir_all(&cwd);
    }
}

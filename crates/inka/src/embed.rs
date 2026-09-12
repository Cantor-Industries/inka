// inka build embed engine: collects the file set that goes into an inka artifact.
//
//   closure  (default): walk static imports from the entry and embed exactly
//                       the referenced files.
//   directory:           embed the whole cwd subtree (minus ignore dirs).
//
// Entries are returned as (path-relative-to-cwd, bytes) pairs.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use deno_ast::swc::ast::{ImportDecl, ModuleDecl, ModuleItem, Program};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Closure,
    Directory,
}

const IGNORE_DIRS: [&str; 5] = [".git", "target", "node_modules", ".inka", "dist"];
const LOCK_FILE: &str = "vendored.lock";

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
    Ok(rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}

pub fn collect(cwd: &Path, entry_rel: &str) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut queue: Vec<String> = vec![entry_rel.to_string()];
    let mut warned = false;

    while let Some(rel) = queue.pop() {
        let abs = cwd.join(&rel);
        if files.contains_key(&rel) {
            continue;
        }
        let bytes = fs::read(&abs).map_err(|e| format!("cannot read {}: {e}", abs.display()))?;
        files.insert(rel.clone(), bytes.clone());

        let specifiers = scan_specifiers(cwd, &rel, &bytes, &mut warned);
        for s in specifiers {
            if let Some(target) = resolve_local(cwd, &rel, &s) {
                if !files.contains_key(&target) {
                    queue.push(target);
                }
            }
            // non-local specifiers (bare / npm: / jsr: / node:) are left for the
            // runtime to resolve against the package store or built-ins — silence.
        }
    }

    // Make sure the entry is always first for deterministic ordering.
    let entry = entry_rel.to_string();
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    if let Some(b) = files.remove(&entry) {
        out.push((entry, b));
    }
    for (k, v) in files {
        out.push((k, v));
    }
    Ok(out)
}

/// Resolve a non-relative specifier to a cwd-relative `vendored/…` file that a
/// `--vendor-closure` build should embed. Walks the pure resolver with the
/// project's `vendored/` as the vendor root and no store: only a
/// `Decision::File` under that root is embeddable; store/builtin/error results
/// (and everything outside `vendored/`) are left for runtime resolution.
fn vendored_target(cwd: &Path, vendor_root: &Path, from_rel: &str, spec: &str) -> Option<String> {
    let referrer = url_from_rel(cwd, from_rel).to_string();
    let decision = inka_resolver::resolve_v2(None, Some(vendor_root), &referrer, spec);
    let inka_resolver::Decision::File(p) = decision else {
        return None;
    };
    if !p.starts_with(vendor_root) {
        return None;
    }
    let rel = p
        .strip_prefix(cwd)
        .ok()?
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    rel.starts_with("vendored/").then_some(rel)
}

/// Find the cwd-relative `package.json` of the vendored package root that owns
/// `rel` (the nearest ancestor dir under `vendored/` that has one), so the
/// runtime resolver can serve the package. None for files outside a root.
fn package_json_for(cwd: &Path, rel: &str) -> Option<String> {
    let mut dir = Path::new(rel).parent();
    while let Some(d) = dir {
        let s = d.to_string_lossy();
        if cwd.join(d).join("package.json").is_file() {
            return Some(format!("{s}/package.json"));
        }
        if s == "vendored" {
            break;
        }
        dir = d.parent();
    }
    None
}

/// Collect only the vendored modules reachable from the entry's import graph
/// (`--vendor-closure`). App files are walked (and returned only when they live
/// under `vendored/`) so relative imports inside vendored packages and further
/// bare imports are followed through the resolver. Store/builtin-only packages
/// are not embedded — they are resolved from the machine default store at run
/// time. Each reached package root's `package.json` is embedded too (the
/// runtime resolver reads it). Entries are cwd-relative `vendored/…` paths, as
/// in `collect_vendored`.
pub fn collect_vendored_closure(
    cwd: &Path,
    entry_rel: &str,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    let vendor_root = cwd.join("vendored");
    if !vendor_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut queue: Vec<String> = vec![entry_rel.to_string()];
    let mut warned = false;

    while let Some(rel) = queue.pop() {
        if !visited.insert(rel.clone()) {
            continue;
        }
        let bytes = fs::read(cwd.join(&rel))
            .map_err(|e| format!("cannot read {}: {e}", cwd.join(&rel).display()))?;
        if rel.starts_with("vendored/") {
            files.insert(rel.clone(), bytes.clone());
            if let Some(pj) = package_json_for(cwd, &rel) {
                if !files.contains_key(&pj) {
                    if let Ok(b) = fs::read(cwd.join(&pj)) {
                        files.insert(pj, b);
                    }
                }
            }
        }
        for s in scan_specifiers(cwd, &rel, &bytes, &mut warned) {
            if let Some(target) = resolve_local(cwd, &rel, &s) {
                if !visited.contains(&target) {
                    queue.push(target);
                }
            } else if let Some(vtarget) = vendored_target(cwd, &vendor_root, &rel, &s) {
                if !visited.contains(&vtarget) {
                    queue.push(vtarget);
                }
            }
            // store / builtin / network specifiers are never embedded.
        }
    }

    Ok(files.into_iter().collect())
}

/// Scan one source file for import specifiers: static imports/export-from via
/// the AST, plus literal `import("...")` calls via a light scan.
fn scan_specifiers(cwd: &Path, rel: &str, bytes: &[u8], warned: &mut bool) -> Vec<String> {
    let mut specs = Vec::new();
    if ext_of(rel).is_some_and(|e| e == "json") {
        return specs;
    }
    let text = String::from_utf8_lossy(bytes).into_owned();

    if let Some(parsed) = parse_program(cwd, rel, &text) {
        let program = parsed.program();
        if let Program::Module(m) = &*program {
            for item in &m.body {
                if let ModuleItem::ModuleDecl(decl) = item {
                    match decl {
                        ModuleDecl::Import(ImportDecl { src, .. }) => {
                            if let Some(s) = src.value.as_str() {
                                specs.push(s.to_string());
                            }
                        }
                        ModuleDecl::ExportNamed(e) => {
                            if let Some(src) = &e.src {
                                if let Some(s) = src.value.as_str() {
                                    specs.push(s.to_string());
                                }
                            }
                        }
                        ModuleDecl::ExportAll(e) => {
                            if let Some(s) = e.src.value.as_str() {
                                specs.push(s.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    specs.extend(scan_dynamic_imports(&text, rel, warned));
    specs
}

fn parse_program(cwd: &Path, rel: &str, text: &str) -> Option<deno_ast::ParsedSource> {
    use deno_ast::{parse_module, MediaType, ParseParams};

    let media = MediaType::from_path(Path::new(rel));
    if matches!(media, MediaType::Json) {
        return None;
    }
    parse_module(ParseParams {
        specifier: url_from_rel(cwd, rel),
        text: text.to_string().into(),
        media_type: media,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })
    .map_err(|e| {
        eprintln!("warning: could not parse {rel} for import analysis: {e}");
    })
    .ok()
}

fn url_from_rel(cwd: &Path, rel: &str) -> deno_ast::ModuleSpecifier {
    deno_ast::ModuleSpecifier::from_file_path(cwd.join(rel))
        .unwrap_or_else(|_| deno_ast::ModuleSpecifier::parse("file:///unknown.ts").unwrap())
}

/// Find `import("...")` calls. Only a fully-literal specifier is treated as an
/// import (embeddable); a computed specifier triggers a warning pointing at
/// `--embed-dir` since closure mode cannot see it.
fn scan_dynamic_imports(text: &str, rel: &str, warned: &mut bool) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 7 <= bytes.len() {
        if &bytes[i..i + 7] == b"import(" || &bytes[i..i + 7] == b"import (" {
            // find opening paren
            let mut j = i + 6;
            while j < bytes.len() && bytes[j] != b'(' {
                j += 1;
            }
            j += 1;
            // skip whitespace
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\n') {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'\'' || bytes[j] == b'"' || bytes[j] == b'`') {
                let q = bytes[j];
                let mut k = j + 1;
                let mut spec = String::new();
                while k < bytes.len() && bytes[k] != q {
                    spec.push(bytes[k] as char);
                    k += 1;
                }
                if k >= bytes.len() {
                    i += 1;
                    continue; // unterminated string; not a real import
                }
                // skip to the closing paren
                let mut k2 = k + 1;
                while k2 < bytes.len()
                    && (bytes[k2] == b' ' || bytes[k2] == b'\t' || bytes[k2] == b'\n')
                {
                    k2 += 1;
                }
                if k2 < bytes.len() && bytes[k2] == b')' {
                    if !spec.is_empty() {
                        out.push(spec);
                    }
                } else if !*warned {
                    eprintln!(
                        "warning: dynamic import in {rel} is not a plain string literal; \
                         closure mode cannot embed it - use --embed-dir to include the whole tree"
                    );
                    *warned = true;
                }
            }
        }
        i += 1;
    }
    out
}

fn ext_of(name: &str) -> Option<String> {
    name.rsplit('.').next().map(|e| e.to_ascii_lowercase())
}

const TRY_EXTS: [&str; 6] = ["ts", "mts", "cts", "js", "mjs", "json"];

/// Resolve a relative specifier to a cwd-relative file path.
fn resolve_local(cwd: &Path, from_rel: &str, spec: &str) -> Option<String> {
    if !(spec.starts_with("./") || spec.starts_with("../") || spec.starts_with("/")) {
        return None; // bare / npm: / jsr: / https: / node: etc. -> external
    }
    let base_dir = Path::new(from_rel).parent().unwrap_or(Path::new(""));
    let candidate = base_dir.join(spec);
    // exact hit
    if let Some(rel) = existing_rel(cwd, &candidate) {
        return Some(rel);
    }
    // try appending extensions
    for ext in TRY_EXTS {
        let with_ext = PathBuf::from(format!("{}.{ext}", candidate.to_string_lossy()));
        if let Some(rel) = existing_rel(cwd, &with_ext) {
            return Some(rel);
        }
    }
    // directory index
    for ext in TRY_EXTS {
        let idx = candidate.join(format!("index.{ext}"));
        if let Some(rel) = existing_rel(cwd, &idx) {
            return Some(rel);
        }
    }
    None
}

fn existing_rel(cwd: &Path, p: &Path) -> Option<String> {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    if !abs.is_file() {
        return None;
    }
    // Canonicalize so any ".."/"." segments are resolved and we report a clean
    // relative path (the launcher rejects ".." components in archive entries).
    let real = fs::canonicalize(&abs).ok()?;
    let rcwd = fs::canonicalize(cwd).ok()?;
    let rel = real.strip_prefix(&rcwd).ok()?;
    let rel = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if rel.is_empty() || rel.starts_with("..") {
        return None;
    }
    Some(rel)
}

/// Whole-cwd directory embed (ignoring common build dirs).
pub fn collect_directory(cwd: &Path, entry_rel: &str) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    walk(cwd, cwd, &mut files)?;
    if !files.contains_key(entry_rel) {
        // entry itself may have been filtered; ensure present
        if let Ok(bytes) = fs::read(cwd.join(entry_rel)) {
            files.insert(entry_rel.to_string(), bytes);
        }
    }
    let entry = entry_rel.to_string();
    let mut out = Vec::new();
    if let Some(b) = files.remove(&entry) {
        out.push((entry, b));
    }
    for (k, v) in files {
        out.push((k, v));
    }
    Ok(out)
}

fn walk(cwd: &Path, dir: &Path, files: &mut BTreeMap<String, Vec<u8>>) -> Result<(), String> {
    let rd = fs::read_dir(dir).map_err(|e| format!("cannot read dir {}: {e}", dir.display()))?;
    for ent in rd.flatten() {
        let ft = ent.file_type().map_err(|e| e.to_string())?;
        let name = ent.file_name().to_string_lossy().into_owned();
        if ft.is_dir() {
            if IGNORE_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk(cwd, &ent.path(), files)?;
        } else if ft.is_file() {
            if let Ok(bytes) = fs::read(ent.path()) {
                if let Ok(rel) = ent.path().strip_prefix(cwd) {
                    let rel = rel
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("/");
                    files.insert(rel, bytes);
                }
            }
        }
    }
    Ok(())
}

/// Collect the per-project vendored package roots (`vendored/<name>/…`) as
/// cwd-relative entries, ready to embed into an artifact (whole-pool mode).
/// The vendored lock/conversion bookkeeping files are not runtime modules.
///
/// Only genuine package roots are embedded. The pool is name-keyed flat:
///   vendored/<name>/package.json                    (bare packages)
///   vendored/@scope/<name>/package.json             (scoped packages)
/// A top-level `vendored/` entry that is not a package root (a stray dir or
/// file, `Go`-style vendor leftovers, etc.) is skipped so it never becomes
/// artifact payload.
pub fn collect_vendored(cwd: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let vendored = cwd.join("vendored");
    if !vendored.is_dir() {
        return Ok(Vec::new());
    }
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for ent in fs::read_dir(&vendored)
        .map_err(|e| format!("cannot read dir {}: {e}", vendored.display()))?
        .flatten()
    {
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue; // hidden entries (and bookkeeping dotfiles) are not packages
        }
        let ft = ent.file_type().map_err(|e| e.to_string())?;
        if !ft.is_dir() {
            continue; // a stray file at the pool root is not a package
        }
        let root = ent.path();
        if root.join("package.json").is_file() {
            walk_vendored(cwd, &root, &mut files)?;
        } else if name.starts_with('@') {
            // scope container (vendored/@scope/<name>/package.json): descend one
            // level and embed only the child dirs that are real package roots.
            for sub in fs::read_dir(&root)
                .map_err(|e| format!("cannot read dir {}: {e}", root.display()))?
                .flatten()
            {
                let ft = sub.file_type().map_err(|e| e.to_string())?;
                if ft.is_dir() && sub.path().join("package.json").is_file() {
                    walk_vendored(cwd, &sub.path(), &mut files)?;
                }
            }
        }
        // anything else: not a package root -> skip entirely.
    }
    Ok(files.into_iter().collect())
}

/// Files that never run and bloat whole-pool artifacts. Vendored packages ship
/// tests/specs, source maps, and type declarations (only tooling uses .d.ts);
/// the app's own files are never filtered (this is only consulted by
/// `walk_vendored`).
fn is_vendor_noise(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    l.ends_with("_test.ts")
        || l.ends_with(".test.ts")
        || l.ends_with(".test.js")
        || l.contains(".spec.")
        || l.ends_with(".map")
        || l.ends_with(".d.ts")
        || l.ends_with(".d.mts")
        || l.ends_with(".d.cts")
}

fn walk_vendored(
    cwd: &Path,
    dir: &Path,
    files: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), String> {
    let rd = fs::read_dir(dir).map_err(|e| format!("cannot read dir {}: {e}", dir.display()))?;
    for ent in rd.flatten() {
        let ft = ent.file_type().map_err(|e| e.to_string())?;
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        // bookkeeping files are not runtime modules
        if name == LOCK_FILE || name == "vendor.json" {
            continue;
        }
        if ft.is_dir() {
            if name == "node_modules" || name == ".git" {
                continue;
            }
            walk_vendored(cwd, &ent.path(), files)?;
        } else if ft.is_file() {
            // WS3-1: skip vendored tests/specs, source maps, and type
            // declarations — never imported at run time.
            if is_vendor_noise(&name) {
                continue;
            }
            if let Ok(bytes) = fs::read(ent.path()) {
                if let Ok(rel) = ent.path().strip_prefix(cwd) {
                    let rel = rel
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("/");
                    files.insert(rel, bytes);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
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
    fn collect_vendored_skips_non_package_entries() {
        let cwd = scratch();
        mk(
            &cwd,
            "vendored/ws/package.json",
            r#"{"name":"ws","version":"1.0.0"}"#,
        );
        mk(&cwd, "vendored/ws/index.js", "export const x = 1;\n");
        // stray dir and stray top-level file: not package roots -> not embedded
        mk(&cwd, "vendored/junk/file.txt", "stray\n");
        mk(&cwd, "vendored/stray.txt", "top-level stray file\n");
        let files = collect_vendored(&cwd).unwrap();
        let rels: Vec<String> = files.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(
            rels,
            vec!["vendored/ws/index.js", "vendored/ws/package.json"],
            "{rels:?}"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn collect_vendored_keeps_scoped_packages() {
        let cwd = scratch();
        mk(
            &cwd,
            "vendored/@effect/platform/package.json",
            r#"{"name":"@effect/platform","version":"1.0.0"}"#,
        );
        mk(
            &cwd,
            "vendored/@effect/platform/lib/mod.ts",
            "export const p = 1;\n",
        );
        // a stray file directly inside a scope container is not a package
        mk(&cwd, "vendored/@junk/note.txt", "not a package\n");
        let files = collect_vendored(&cwd).unwrap();
        let rels: Vec<String> = files.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(
            rels,
            vec![
                "vendored/@effect/platform/lib/mod.ts",
                "vendored/@effect/platform/package.json",
            ],
            "{rels:?}"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // --vendor-closure: only the vendored modules reachable from the entry are
    // embedded (walking through vendored relative imports); unrelated vendored
    // packages and the app's own files are excluded.
    #[test]
    fn collect_vendored_closure_embeds_only_reachable_vendored() {
        let cwd = scratch();
        mk(
            &cwd,
            "app.js",
            "import { once } from \"onetime\";\nimport \"./util.js\";\nconsole.log(once);\n",
        );
        mk(&cwd, "util.js", "export const u = 1;\n");
        mk(
            &cwd,
            "vendored/onetime/package.json",
            r#"{"name":"onetime","version":"7.2.0","type":"module","exports":{".":"./index.js"}}"#,
        );
        mk(
            &cwd,
            "vendored/onetime/index.js",
            "import { x } from \"./lib/x.js\";\nexport function once() { return x; }\n",
        );
        mk(&cwd, "vendored/onetime/lib/x.js", "export const x = 1;\n");
        // unrelated vendored package nothing imports
        mk(
            &cwd,
            "vendored/extra/package.json",
            r#"{"name":"extra","version":"1.0.0","type":"module","main":"index.js"}"#,
        );
        mk(&cwd, "vendored/extra/index.js", "export const e = 2;\n");

        let files = collect_vendored_closure(&cwd, "app.js").unwrap();
        let rels: Vec<String> = files.iter().map(|(r, _)| r.clone()).collect();
        for want in [
            "vendored/onetime/index.js",
            "vendored/onetime/lib/x.js",
            "vendored/onetime/package.json",
        ] {
            assert!(rels.contains(&want.to_string()), "missing {want}: {rels:?}");
        }
        assert!(
            rels.iter().all(|r| !r.starts_with("vendored/extra")),
            "{rels:?}"
        );
        assert!(
            rels.iter().all(|r| r != "app.js" && r != "util.js"),
            "app files must not be returned: {rels:?}"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // WS3-1: whole-pool vendored embed skips tests/specs, source maps, and type
    // declarations, but keeps real source (including .ts) modules.
    #[test]
    fn collect_vendored_skips_noise_files() {
        let cwd = scratch();
        mk(
            &cwd,
            "vendored/ws/package.json",
            r#"{"name":"ws","version":"1.0.0"}"#,
        );
        mk(&cwd, "vendored/ws/index.js", "export const i = 1;\n");
        mk(&cwd, "vendored/ws/mod.ts", "export const t = 1;\n"); // real .ts source kept
        mk(&cwd, "vendored/ws/lib_test.ts", "export const t = 1;\n");
        mk(&cwd, "vendored/ws/spec.test.js", "export const t = 1;\n");
        mk(&cwd, "vendored/ws/y.spec.ts", "export const t = 1;\n");
        mk(&cwd, "vendored/ws/index.js.map", "{}");
        mk(
            &cwd,
            "vendored/ws/index.d.ts",
            "export declare const i: number;\n",
        );
        let files = collect_vendored(&cwd).unwrap();
        let rels: Vec<String> = files.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(
            rels,
            vec![
                "vendored/ws/index.js",
                "vendored/ws/mod.ts",
                "vendored/ws/package.json",
            ],
            "{rels:?}"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }
}

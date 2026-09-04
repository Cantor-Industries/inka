// inka build embed engine: collects the file set that goes into an inka artifact.
//
//   closure  (default): walk static imports from the entry and embed exactly
//                       the referenced files.
//   directory:           embed the whole cwd subtree (minus ignore dirs).
//
// Entries are returned as (path-relative-to-cwd, bytes) pairs.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use deno_ast::swc::ast::{ImportDecl, ModuleDecl, ModuleItem, Program};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Closure,
    Directory,
}

const IGNORE_DIRS: [&str; 5] = [".git", "target", "node_modules", ".inka", "dist"];

pub fn rel_from_cwd(cwd: &Path, p: &Path) -> Result<String, String> {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    let canon = fs::canonicalize(&abs)
        .map_err(|e| format!("cannot resolve {}: {e}", abs.display()))?;
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
        let bytes = fs::read(&abs)
            .map_err(|e| format!("cannot read {}: {e}", abs.display()))?;
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
    use deno_ast::{MediaType, ParseParams, parse_module};

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
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\n')
            {
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
                while k2 < bytes.len() && (bytes[k2] == b' ' || bytes[k2] == b'\t' || bytes[k2] == b'\n')
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
    let abs = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
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

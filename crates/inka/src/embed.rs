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
            // runtime to resolve against node_modules or built-ins — silence.
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

/// Split a bare/`npm:`/`jsr:` specifier into `(npm identity name, subpath)`.
/// Schemes (other than npm:/jsr:) and relative/absolute specifiers are not
/// node_modules targets.
fn package_spec(spec: &str) -> Option<(String, Option<String>)> {
    if spec.starts_with("npm:") || spec.starts_with("jsr:") {
        let ps = parse_pkg_specifier(spec).ok()?;
        return Some((ps.name, ps.sub));
    }
    if spec.contains(':')
        || spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with('/')
    {
        return None;
    }
    Some(split_bare(spec))
}

/// One parsed `npm:`/`jsr:` specifier, normalized to its npm identity.
struct PkgSpec {
    name: String,
    sub: Option<String>,
}

fn parse_pkg_specifier(spec: &str) -> Result<PkgSpec, String> {
    let body = if let Some(rest) = spec.strip_prefix("npm:") {
        rest.to_string()
    } else if let Some(rest) = spec.strip_prefix("jsr:") {
        let rest = rest.trim();
        let (scope, after) = rest
            .split_once('/')
            .ok_or_else(|| format!("invalid jsr specifier '{spec}'"))?;
        let scope = scope.strip_prefix('@').unwrap_or(scope);
        let (name, tail) = split_name_suffix(after);
        format!("@jsr/{scope}__{name}{tail}")
    } else {
        return Err(format!("not a package specifier: '{spec}'"));
    };
    parse_npm_body(&body)
}

fn split_name_suffix(after: &str) -> (&str, &str) {
    match after.find(['@', '/']) {
        Some(i) => (&after[..i], &after[i..]),
        None => (after, ""),
    }
}

fn parse_npm_body(body: &str) -> Result<PkgSpec, String> {
    let body = body.trim();
    let (name, rest) = if body.starts_with('@') {
        let (scope, after) = body
            .split_once('/')
            .ok_or_else(|| format!("malformed scoped package '{body}'"))?;
        let (nm, rest) = split_name_suffix(after);
        (format!("{scope}/{nm}"), rest)
    } else {
        let (nm, rest) = split_name_suffix(body);
        (nm.to_string(), rest)
    };
    let mut sub = None;
    if let Some(tail) = rest.strip_prefix('@') {
        if let Some((_req, s)) = tail.split_once('/') {
            sub = Some(s.to_string());
        }
    } else if let Some(s) = rest.strip_prefix('/') {
        sub = Some(s.to_string());
    }
    Ok(PkgSpec { name, sub })
}

/// Package identities a bare name may map to (jsr mirror included).
fn package_identities(name: &str) -> Vec<String> {
    let mut out = vec![name.to_string()];
    if let Some(body) = name.strip_prefix('@') {
        if let Some((scope, pkg)) = body.split_once('/') {
            if scope != "jsr" {
                out.push(format!("@jsr/{scope}__{pkg}"));
            }
        }
    }
    out
}

/// Splits a bare specifier into `(package name, optional subpath)`.
fn split_bare(spec: &str) -> (String, Option<String>) {
    if spec.starts_with('@') {
        if let Some((head, tail)) = spec.split_once('/') {
            return match tail.split_once('/') {
                Some((name, rest)) => (format!("{head}/{name}"), Some(rest.to_string())),
                None => (format!("{head}/{tail}"), None),
            };
        }
        (spec.to_string(), None)
    } else {
        match spec.split_once('/') {
            Some((n, rest)) => (n.to_string(), Some(rest.to_string())),
            None => (spec.to_string(), None),
        }
    }
}

/// ESM-capable `exports` conditions, in priority order.
const EXPORT_CONDITIONS: [&str; 3] = ["import", "node", "default"];

/// Resolve a package's entry (or subpath) to a file inside `pkg_root`.
fn resolve_pkg_file(pkg_root: &Path, subpath: Option<&str>) -> Result<PathBuf, String> {
    let pkg_json_path = pkg_root.join("package.json");
    let raw = fs::read_to_string(&pkg_json_path)
        .map_err(|e| format!("cannot read {}: {e}", pkg_json_path.display()))?;
    let pkg: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("invalid package.json in {}: {e}", pkg_json_path.display()))?;
    let sub = subpath.unwrap_or("").trim_start_matches("./");

    let target = match pkg.get("exports") {
        Some(serde_json::Value::Null) | None => legacy_package_target(pkg_root, &pkg, sub)?,
        Some(exports) => exports_target(exports, sub)?,
    };
    let target = target.trim_start_matches("./");
    if Path::new(target)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!(
            "export target '{target}' escapes the package directory"
        ));
    }
    let file = pkg_root.join(target);
    if !file.is_file() {
        return Err(format!(
            "package module not found on disk: {}",
            file.display()
        ));
    }
    Ok(file)
}

fn exports_target(exports: &serde_json::Value, subpath: &str) -> Result<String, String> {
    use serde_json::Value;

    fn pick_conditions(v: &Value) -> Result<String, String> {
        match v {
            Value::String(s) => Ok(s.clone()),
            Value::Array(items) => {
                for item in items {
                    if let Ok(x) = pick_conditions(item) {
                        return Ok(x);
                    }
                }
                Err("no usable export target".to_string())
            }
            Value::Object(map) => {
                for cond in EXPORT_CONDITIONS {
                    if let Some(val) = map.get(cond) {
                        return pick_conditions(val);
                    }
                }
                Err("package has no import/default export target".to_string())
            }
            _ => Err("malformed exports target".to_string()),
        }
    }

    let sub = subpath.trim_start_matches("./");
    let is_map = match exports {
        Value::Object(map) => map.keys().any(|k| k == "." || k.starts_with("./")),
        _ => false,
    };
    let target = if is_map {
        let map = exports.as_object().unwrap();
        if sub.is_empty() {
            match map.get(".") {
                Some(v) => pick_conditions(v)?,
                None => return Err("package has no '.' export".to_string()),
            }
        } else if let Some(v) = map.get(format!("./{sub}").as_str()) {
            pick_conditions(v)?
        } else {
            let mut hit = None;
            for (k, v) in map {
                if let Some(star) = k.strip_suffix('*') {
                    let prefix = star.strip_prefix("./").unwrap_or(star);
                    if let Some(rem) = sub.strip_prefix(prefix) {
                        hit = Some(pick_conditions(v)?.replace('*', rem));
                        break;
                    }
                }
            }
            match hit {
                Some(t) => t,
                None => return Err(format!("no exported subpath './{sub}' for this package")),
            }
        }
    } else if sub.is_empty() {
        pick_conditions(exports)?
    } else {
        return Err(format!("no exported subpath './{sub}' for this package"));
    };
    Ok(target)
}

fn legacy_package_target(
    pkg_root: &Path,
    pkg: &serde_json::Value,
    sub: &str,
) -> Result<String, String> {
    let resolve_loose = |rel: &str| -> Option<String> {
        let rel = rel.trim_start_matches("./");
        let candidate = pkg_root.join(rel);
        if candidate.is_file() {
            return Some(rel.to_string());
        }
        const EXTS: [&str; 3] = ["js", "mjs", "json"];
        for ext in EXTS {
            let cand = PathBuf::from(format!("{rel}.{ext}"));
            if pkg_root.join(&cand).is_file() {
                return Some(format!("{rel}.{ext}"));
            }
        }
        if candidate.is_dir() {
            for idx in ["index.js", "index.mjs", "index.json"] {
                if candidate.join(idx).is_file() {
                    return Some(format!("{rel}/{idx}"));
                }
            }
        }
        None
    };
    if sub.is_empty() {
        if let Some(main) = pkg.get("main").and_then(|v| v.as_str()) {
            if let Some(t) = resolve_loose(main) {
                return Ok(t);
            }
        }
        return resolve_loose("index.js").ok_or_else(|| "package has no main entry".to_string());
    }
    resolve_loose(sub).ok_or_else(|| format!("cannot resolve file '{sub}' in package"))
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
        if s == "vendored" || d.file_name().is_some_and(|n| n == "node_modules") {
            break;
        }
        dir = d.parent();
    }
    None
}

/// Collect the project's `node_modules` files reachable from the entry graph
/// (bring-your-own-node_modules). Bare/`npm:`/`jsr:` specifiers resolve against
/// `<cwd>/node_modules` (nearest `node_modules` first, then the hoisted root);
/// builtin specifiers are left for runtime resolution. Files are embedded
/// at their `node_modules/…` paths (through symlinks, so Deno's isolated
/// `.deno/` and pnpm's `.pnpm/` layouts work), and each reached package root's
/// `package.json` is embedded too.
pub fn collect_node_modules_closure(
    cwd: &Path,
    entry_rel: &str,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    let nm = cwd.join("node_modules");
    if !nm.is_dir() {
        return Ok(Vec::new());
    }
    collect_closure(cwd, entry_rel, &nm, "node_modules/")
}

/// Walk the import graph from `entry_rel`, embedding reached files under the
/// `node_modules` tree at `nm` (whose cwd-relative prefix is `prefix`).
fn collect_closure(
    cwd: &Path,
    entry_rel: &str,
    nm: &Path,
    prefix: &str,
) -> Result<Vec<(String, Vec<u8>)>, String> {
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
        let in_nm = rel.starts_with(prefix);
        if in_nm {
            files.insert(rel.clone(), bytes.clone());
            if let Some(pj) = package_json_for(cwd, &rel) {
                if let Ok(b) = fs::read(cwd.join(&pj)) {
                    files.entry(pj).or_insert(b);
                }
            }
        }
        for s in scan_specifiers(cwd, &rel, &bytes, &mut warned) {
            let target = if in_nm {
                resolve_local_raw(cwd, &rel, &s).or_else(|| node_modules_target(cwd, nm, &rel, &s))
            } else {
                resolve_local(cwd, &rel, &s).or_else(|| node_modules_target(cwd, nm, &rel, &s))
            };
            if let Some(t) = target {
                if !visited.contains(&t) {
                    queue.push(t);
                }
            }
        }
    }

    Ok(files.into_iter().collect())
}

/// Resolve a bare/`npm:`/`jsr:` specifier to a file under `<cwd>/node_modules`,
/// walking the nearest `node_modules` from `from_rel` first (nested wins over
/// hoisted), then the hoisted root. Returns a cwd-relative path (symlinks
/// preserved).
fn node_modules_target(cwd: &Path, nm: &Path, from_rel: &str, spec: &str) -> Option<String> {
    let (name, sub) = package_spec(spec)?;
    let identities = package_identities(&name);
    let from_abs = cwd.join(from_rel);
    let tree = nm.parent().unwrap_or(nm);
    let mut dir = from_abs.parent();
    while let Some(d) = dir {
        if !d.starts_with(tree) {
            break;
        }
        for ident in &identities {
            let pkg = d.join("node_modules").join(ident);
            if pkg.join("package.json").is_file() {
                if let Ok(file) = resolve_pkg_file(&pkg, sub.as_deref()) {
                    if let Some(rel) = rel_of(cwd, &file) {
                        return Some(rel);
                    }
                }
            }
        }
        dir = d.parent();
    }
    // Hoisted root, for referrers outside this node_modules tree (e.g. app code
    // importing a vendored package).
    for ident in &identities {
        let pkg = nm.join(ident);
        if pkg.join("package.json").is_file() {
            if let Ok(file) = resolve_pkg_file(&pkg, sub.as_deref()) {
                if let Some(rel) = rel_of(cwd, &file) {
                    return Some(rel);
                }
            }
        }
    }
    None
}

/// Like `resolve_local`, but keeps the referrer's path (no canonicalization) so
/// files reached through a `node_modules/<pkg>` symlink stay at the symlink path
/// the runtime resolves.
fn resolve_local_raw(cwd: &Path, from_rel: &str, spec: &str) -> Option<String> {
    if !(spec.starts_with("./") || spec.starts_with("../")) {
        return None;
    }
    let base_dir = Path::new(from_rel).parent().unwrap_or(Path::new(""));
    let candidate = normalize_rel(&base_dir.join(spec));
    if let Some(r) = rel_of(cwd, &candidate) {
        if cwd.join(&candidate).is_file() {
            return Some(r);
        }
    }
    for ext in TRY_EXTS {
        let with_ext = PathBuf::from(format!("{}.{ext}", candidate.to_string_lossy()));
        if cwd.join(&with_ext).is_file() {
            if let Some(r) = rel_of(cwd, &with_ext) {
                return Some(r);
            }
        }
    }
    for ext in TRY_EXTS {
        let idx = candidate.join(format!("index.{ext}"));
        if cwd.join(&idx).is_file() {
            if let Some(r) = rel_of(cwd, &idx) {
                return Some(r);
            }
        }
    }
    None
}

/// Lexically resolve `.`/`..` segments (no filesystem access).
fn normalize_rel(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// cwd-relative path string for `p`: an absolute path is stripped against
/// `cwd`, a relative one is used as-is. None when empty or escaping via `..`.
fn rel_of(cwd: &Path, p: &Path) -> Option<String> {
    let rel: PathBuf = if p.is_absolute() {
        p.strip_prefix(cwd).ok()?.to_path_buf()
    } else {
        p.to_path_buf()
    };
    let s = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if s.is_empty() || s.starts_with("..") {
        None
    } else {
        Some(s)
    }
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
    specs.extend(scan_require_calls(&text));
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

/// Find literal `require("...")` calls. The engine runs CommonJS natively and
/// vendored packages ship raw, so closure mode must follow CJS `require()` too
/// (not just ESM `import`). Only a plain single/double-quoted string literal is
/// embedded; computed specifiers are left for the runtime store (or
/// `--embed-dir`). Textual scan: `parse_module` rejects some CJS files, and a
/// missed `require` is worse than an occasional false positive (a specifier
/// that resolves to nothing is simply ignored).
fn scan_require_calls(text: &str) -> Vec<String> {
    const NEEDLE: &[u8] = b"require";
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + NEEDLE.len() <= bytes.len() {
        if &bytes[i..i + NEEDLE.len()] != NEEDLE {
            i += 1;
            continue;
        }
        // Standalone identifier only: skip `x.require(` / `myrequire(`.
        let prev = i.checked_sub(1).map(|p| bytes[p]);
        let standalone = !matches!(
            prev,
            Some(c) if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c == b'.'
        );
        let mut j = i + NEEDLE.len();
        while standalone && j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        if standalone && j < bytes.len() && bytes[j] == b'(' {
            j += 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'\'' || bytes[j] == b'"') {
                let quote = bytes[j];
                let mut k = j + 1;
                let mut spec = String::new();
                let mut closed = false;
                while k < bytes.len() {
                    if bytes[k] == b'\\' {
                        k += 2;
                        continue;
                    }
                    if bytes[k] == quote {
                        closed = true;
                        break;
                    }
                    spec.push(bytes[k] as char);
                    k += 1;
                }
                if closed && !spec.is_empty() {
                    out.push(spec);
                    i = k + 1;
                    continue;
                }
            }
        }
        i += NEEDLE.len();
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
    fn node_modules_closure_follows_relative_require() {
        let cwd = scratch();
        mk(
            &cwd,
            "node_modules/pkg/package.json",
            r#"{"name":"pkg","version":"1.0.0","main":"index.js"}"#,
        );
        mk(
            &cwd,
            "node_modules/pkg/index.js",
            "\tmodule.exports = require('./node.js');\n",
        );
        mk(&cwd, "node_modules/pkg/node.js", "module.exports = 42;\n");
        mk(
            &cwd,
            "app.js",
            "const p = require('pkg');\nconsole.log(p);\n",
        );
        let files = collect_node_modules_closure(&cwd, "app.js").unwrap();
        let rels: Vec<String> = files.iter().map(|(r, _)| r.clone()).collect();
        assert!(
            rels.contains(&"node_modules/pkg/index.js".to_string()),
            "{rels:?}"
        );
        assert!(
            rels.contains(&"node_modules/pkg/node.js".to_string()),
            "{rels:?}"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn scan_require_calls_finds_only_standalone_literals() {
        let text = r#"
            const a = require("ms");
            const b = require('debug');
            const c = require(`tpl`);
            const d = require(name);
            obj.require("no");
            myrequire("no");
            require.resolve("no");
        "#;
        assert_eq!(
            scan_require_calls(text),
            vec!["ms".to_string(), "debug".to_string()]
        );
    }
}

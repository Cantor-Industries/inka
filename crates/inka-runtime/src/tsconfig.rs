//! Minimal `tsconfig.json`/`jsconfig.json` resolution for bare specifiers.
//!
//! TypeScript and Bun resolve a bare specifier such as `src/util` against the
//! nearest `tsconfig.json`'s `compilerOptions.baseUrl` (and `paths`). The Deno
//! `node_resolver` used for npm does not read tsconfig, so `src` would otherwise
//! be mistaken for an npm package and fail with "Could not find package 'src'".
//!
//! This module reproduces the `baseUrl`/`paths` lookup (honoring `extends` and
//! JSONC comments/trailing commas) and confines every result to the execution
//! tree's real path, so a tsconfig can never widen module reads.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde_json::Value;
use url::Url;

/// Extensions probed for a tsconfig target, in TypeScript's preference order.
const EXTS: [&str; 9] = ["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "json"];

/// The subset of `compilerOptions` that affects bare-specifier resolution.
#[derive(Default)]
struct TsConfig {
    /// `baseUrl`, made absolute against the declaring config.
    base_url: Option<PathBuf>,
    /// `paths` patterns and their targets (as written, `*` placeholders intact).
    paths: Vec<(String, Vec<String>)>,
    /// Directory that `paths` targets are joined to (`baseUrl`, else the config
    /// dir of the file that declared them).
    paths_base: PathBuf,
}

/// Per-referrer tsconfig resolver. Caches the nearest config by directory.
pub(crate) struct Resolver {
    /// Canonical execution root; resolved files must stay under it.
    root: PathBuf,
    cache: RefCell<HashMap<PathBuf, Option<Rc<TsConfig>>>>,
}

impl Resolver {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root,
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// Resolve a bare `specifier` imported from `referrer` via the nearest
    /// tsconfig's `paths`/`baseUrl`. Returns `None` when no config applies, the
    /// specifier does not map, or the target is missing or outside the tree.
    pub(crate) fn resolve(&self, specifier: &str, referrer: &Url) -> Option<Url> {
        let ref_path = referrer.to_file_path().ok()?;
        let dir = ref_path.parent()?;
        let cfg = self.config_for(dir)?;
        let candidate = candidate_for(&cfg, specifier)?;
        let real = std::fs::canonicalize(&candidate).ok()?;
        if !real.starts_with(&self.root) {
            return None;
        }
        Url::from_file_path(&real).ok()
    }

    fn config_for(&self, dir: &Path) -> Option<Rc<TsConfig>> {
        if let Some(hit) = self.cache.borrow().get(dir) {
            return hit.clone();
        }
        let found = find_config(dir).and_then(|p| load_config(&p).map(Rc::new));
        self.cache
            .borrow_mut()
            .insert(dir.to_path_buf(), found.clone());
        found
    }
}

/// Walk up from `dir` to the first `tsconfig.json`/`jsconfig.json`.
fn find_config(dir: &Path) -> Option<PathBuf> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        for name in ["tsconfig.json", "jsconfig.json"] {
            let p = d.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
        cur = d.parent();
    }
    None
}

/// Match a `specifier` against a `paths` pattern (`*` captures any substring).
fn match_pattern(pattern: &str, specifier: &str) -> Option<String> {
    match pattern.split_once('*') {
        Some((prefix, suffix)) => {
            if specifier.len() >= prefix.len() + suffix.len()
                && specifier.starts_with(prefix)
                && specifier.ends_with(suffix)
            {
                Some(specifier[prefix.len()..specifier.len() - suffix.len()].to_string())
            } else {
                None
            }
        }
        None => (pattern == specifier).then(String::new),
    }
}

/// The first existing file for `specifier` under the config's `paths`/`baseUrl`.
fn candidate_for(cfg: &TsConfig, specifier: &str) -> Option<PathBuf> {
    for (pattern, targets) in &cfg.paths {
        let Some(star) = match_pattern(pattern, specifier) else {
            continue;
        };
        for target in targets {
            let substituted = target.replace('*', &star);
            if let Some(found) = probe(&cfg.paths_base.join(substituted)) {
                return Some(found);
            }
        }
    }
    if let Some(base) = &cfg.base_url {
        if let Some(found) = probe(&base.join(specifier)) {
            return Some(found);
        }
    }
    None
}

/// Probe a path as a file, with each supported extension, or as a directory
/// index file.
fn probe(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    for ext in EXTS {
        let mut candidate = path.as_os_str().to_owned();
        candidate.push(".");
        candidate.push(ext);
        let candidate = PathBuf::from(candidate);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    for ext in EXTS {
        let candidate = path.join(format!("index.{ext}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Resolve an `extends` value (relative path, absolute path, or node module).
fn resolve_extends(config_dir: &Path, extends: &str) -> Option<PathBuf> {
    let is_path = extends.starts_with('.') || extends.starts_with('/');
    if is_path {
        let base = config_dir.join(extends);
        return [
            base.clone(),
            base.with_extension("json"),
            base.join("tsconfig.json"),
        ]
        .into_iter()
        .find(|p| p.is_file());
    }
    // Node-module style: walk ancestors looking in `node_modules`.
    let mut cur = Some(config_dir);
    while let Some(d) = cur {
        let base = d.join("node_modules").join(extends);
        for candidate in [
            base.clone(),
            base.with_extension("json"),
            base.join("tsconfig.json"),
        ] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        cur = d.parent();
    }
    None
}

/// Load a config, merging `extends` (parent first, child wins). JSONC comments
/// and trailing commas are accepted.
fn load_config(path: &Path) -> Option<TsConfig> {
    load_config_inner(path, &mut Vec::new())
}

fn load_config_inner(path: &Path, seen: &mut Vec<PathBuf>) -> Option<TsConfig> {
    let canon = std::fs::canonicalize(path).ok()?;
    if seen.contains(&canon) {
        return None; // `extends` cycle
    }
    seen.push(canon);

    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&strip_jsonc(&text)).ok()?;
    let config_dir = path.parent()?.to_path_buf();

    let mut cfg = TsConfig::default();
    if let Some(ext) = value.get("extends").and_then(|v| v.as_str()) {
        if let Some(parent_path) = resolve_extends(&config_dir, ext) {
            if let Some(parent) = load_config_inner(&parent_path, seen) {
                cfg = parent;
            }
        }
    }

    let compiler_options = value.get("compilerOptions");
    if let Some(base) = compiler_options
        .and_then(|c| c.get("baseUrl"))
        .and_then(|v| v.as_str())
    {
        cfg.base_url = Some(config_dir.join(base));
    }
    if let Some(paths) = compiler_options
        .and_then(|c| c.get("paths"))
        .and_then(|v| v.as_object())
    {
        let mut parsed = Vec::new();
        for (pattern, value) in paths {
            if let Some(arr) = value.as_array() {
                let targets: Vec<String> = arr
                    .iter()
                    .filter_map(|t| t.as_str().map(str::to_string))
                    .collect();
                if !targets.is_empty() {
                    parsed.push((pattern.clone(), targets));
                }
            }
        }
        if !parsed.is_empty() {
            cfg.paths = parsed;
            cfg.paths_base = cfg.base_url.clone().unwrap_or_else(|| config_dir.clone());
        }
    } else if cfg.paths.is_empty() {
        // No paths anywhere; baseUrl (if inherited) is still honored.
    }
    Some(cfg)
}

/// Strip `//` and `/* … */` comments and trailing commas from JSONC text,
/// respecting string literals and escapes.
fn strip_jsonc(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    let mut in_string = false;
    while i < n {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < n {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
                i += 1;
            }
            '/' if i + 1 < n && chars[i + 1] == '/' => {
                while i < n && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if i + 1 < n && chars[i + 1] == '*' => {
                i += 2;
                while i + 1 < n && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i = (i + 2).min(n);
            }
            ',' => {
                let mut j = i + 1;
                loop {
                    while j < n && chars[j].is_whitespace() {
                        j += 1;
                    }
                    if j + 1 < n && chars[j] == '/' && chars[j + 1] == '/' {
                        while j < n && chars[j] != '\n' {
                            j += 1;
                        }
                        continue;
                    }
                    if j + 1 < n && chars[j] == '/' && chars[j + 1] == '*' {
                        j += 2;
                        while j + 1 < n && !(chars[j] == '*' && chars[j + 1] == '/') {
                            j += 1;
                        }
                        j = (j + 2).min(n);
                        continue;
                    }
                    break;
                }
                if j < n && (chars[j] == '}' || chars[j] == ']') {
                    i += 1; // drop the trailing comma
                } else {
                    out.push(c);
                    i += 1;
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, contents).unwrap();
        p
    }

    fn unique(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("inka-tscfg-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        base
    }

    #[test]
    fn match_pattern_wildcards() {
        assert_eq!(match_pattern("src/*", "src/util"), Some("util".to_string()));
        assert_eq!(match_pattern("@/*", "@/a/b"), Some("a/b".to_string()));
        assert_eq!(match_pattern("exact", "exact"), Some(String::new()));
        assert_eq!(match_pattern("exact", "nope"), None);
        assert_eq!(match_pattern("src/*", "src"), None);
    }

    #[test]
    fn strip_jsonc_comments_and_trailing_commas() {
        let parsed: Value =
            serde_json::from_str(&strip_jsonc("{ // c\n \"a\": [1, /* x */ 2,], }")).unwrap();
        assert_eq!(parsed["a"][1], 2);
    }

    #[test]
    fn base_url_resolves_extensionless_and_index() {
        let base = unique("baseurl");
        let pkg = base.join("pkg");
        write(
            &pkg,
            "tsconfig.json",
            r#"{ "compilerOptions": { "baseUrl": "." } }"#,
        );
        write(&pkg.join("src"), "util.ts", "export const u = 1;");
        write(&pkg.join("lib"), "index.ts", "export const l = 1;");

        let root = std::fs::canonicalize(&pkg).unwrap();
        let r = Resolver::new(root);
        let referrer = Url::from_file_path(pkg.join("src/main.ts")).unwrap();

        let util = r.resolve("src/util", &referrer).expect("baseUrl file");
        assert!(util.to_file_path().unwrap().ends_with("src/util.ts"));
        let lib = r.resolve("lib", &referrer).expect("baseUrl index");
        assert!(lib.to_file_path().unwrap().ends_with("lib/index.ts"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn paths_alias_resolves() {
        let base = unique("paths");
        let pkg = base.join("pkg");
        write(
            &pkg,
            "tsconfig.json",
            r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@/*": ["src/*"] } } }"#,
        );
        write(&pkg.join("src"), "util.ts", "export const u = 1;");

        let root = std::fs::canonicalize(&pkg).unwrap();
        let r = Resolver::new(root);
        let referrer = Url::from_file_path(pkg.join("main.ts")).unwrap();
        let hit = r.resolve("@/util", &referrer).expect("paths alias");
        assert!(hit.to_file_path().unwrap().ends_with("src/util.ts"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn extends_is_merged_and_confined() {
        let base = unique("extends");
        write(
            &base,
            "tsconfig.base.json",
            r#"{ "compilerOptions": { "baseUrl": "." } }"#,
        );
        let pkg = base.join("pkg");
        write(
            &pkg,
            "tsconfig.json",
            r#"{ "extends": "../tsconfig.base.json" }"#,
        );
        write(&pkg.join("src"), "util.ts", "export const u = 1;");

        // Rooted at the package: the base config (outside) must not escape it.
        let root = std::fs::canonicalize(&pkg).unwrap();
        let r = Resolver::new(root);
        let referrer = Url::from_file_path(pkg.join("main.ts")).unwrap();
        // baseUrl resolves against the *base config's* dir, so `src/util` points
        // at `base/src/util` (outside the package) and is refused.
        assert!(r.resolve("src/util", &referrer).is_none());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn confinement_rejects_out_of_tree() {
        let base = unique("confine");
        let pkg = base.join("pkg");
        let outside = base.join("outside");
        write(
            &pkg,
            "tsconfig.json",
            r#"{ "compilerOptions": { "baseUrl": ".." } }"#,
        );
        write(&outside, "secret.ts", "export const s = 1;");

        let root = std::fs::canonicalize(&pkg).unwrap();
        let r = Resolver::new(root);
        let referrer = Url::from_file_path(pkg.join("main.ts")).unwrap();
        assert!(r.resolve("outside/secret", &referrer).is_none());

        let _ = std::fs::remove_dir_all(&base);
    }
}

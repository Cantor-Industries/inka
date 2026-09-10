// inka resolver: the import-resolution/policy engine, as a standalone cdylib.
//
// Lives OUTSIDE the V8/deno_runtime cdylib so that resolver changes rebuild in
// seconds instead of relinking the heavy runtime. It is stateless and pure
// Rust + serde (no deno_core dependency). The engine dlopens this library and
// calls inka_resolver_resolve() for every module specifier; a "UseDefault"
// decision tells the engine to run deno_core's own resolve_import for
// relative/file/node:/data: imports.
//
// Versioning: the resolver is its own tuple (libinka_resolver-<v>.so). Its
// ABI number (inka_resolver_abi) is the compatibility contract with the
// engine; bump it on any breaking change to the C surface or the semantics
// the engine relies on.

use std::ffi::{c_char, c_int, CStr, CString};
use std::path::{Path, PathBuf};

use deno_semver::{Version, VersionReq};
use serde_json::Value;

/// Allow-listed condition keys for package.json `exports` target selection.
/// `types` and `require` (CommonJS) are deliberately skipped; only ESM-capable
/// targets are used.
const EXPORT_CONDITIONS: [&str; 3] = ["import", "node", "default"];

/// Result of classifying + resolving a module specifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Let the engine run `deno_core::resolve_import` (relative, `/`, and the
    /// `node:`/`file:`/`data:` schemes).
    UseDefault,
    /// A concrete file to serve (store package resolved through `exports`).
    File(PathBuf),
    /// A Node built-in to load, full specifier like `node:vm`.
    Builtin(String),
    /// Fail with this message.
    Error(String),
}

/// Resolve a module specifier to a decision against the store pool only
/// (no vendored tier). Equivalent to `resolve_v2(store, None, …)`.
pub fn resolve(store: Option<&Path>, referrer: &str, specifier: &str) -> Decision {
    resolve_v2(store, None, referrer, specifier)
}

/// Two-tier resolution:
///   - a referrer under the default `store` root is STORE tier: today's exact
///     semantics (store pool + builtins first; never consults the project's
///     vendored/);
///   - any other referrer (user code, vendored code) is the APP/VENDOR tier:
///     bare and pinned specifiers resolve vendored -> store -> builtins.
pub fn resolve_v2(
    store: Option<&Path>,
    vendor: Option<&Path>,
    referrer: &str,
    specifier: &str,
) -> Decision {
    let in_store = match (store, referrer_file_path(referrer)) {
        (Some(s), Some(p)) => p.starts_with(s),
        _ => false,
    };
    if in_store {
        store_tier_resolve(store.unwrap(), referrer, specifier)
    } else {
        app_vendor_resolve(store, vendor, referrer, specifier)
    }
}

/// Resolution as seen from inside a default-store package. Preserves today's
/// behavior exactly: pinned -> store pool; relative/schemes -> default; bare
/// builtins core-win; bare packages resolve through the nearest node_modules.
fn store_tier_resolve(store: &Path, referrer: &str, specifier: &str) -> Decision {
    if specifier.starts_with("npm:") || specifier.starts_with("jsr:") {
        return match store_lookup(store, specifier) {
            Ok(path) => Decision::File(path),
            Err(e) => Decision::Error(e),
        };
    }
    if has_scheme(specifier) {
        if specifier.starts_with("http://") || specifier.starts_with("https://") {
            return Decision::Error(format!(
                "network module imports are disabled ('{specifier}'); \
                 vendor the package with `inka update` instead"
            ));
        }
        return Decision::UseDefault;
    }
    if specifier.starts_with("./") || specifier.starts_with("../") || specifier.starts_with('/') {
        return Decision::UseDefault;
    }
    // Bare Node built-ins resolve without the `node:` prefix (core wins).
    if let Some(node_spec) = node_builtin_spec(specifier) {
        return Decision::Builtin(node_spec);
    }
    if let Some(ref_path) = referrer_file_path(referrer) {
        // inside a store package: resolve its installed dependency closure
        return match store_bare_lookup(store, specifier, &ref_path) {
            Ok(path) => Decision::File(path),
            Err(e) => Decision::Error(e),
        };
    }
    match store_bare_top(store, specifier) {
        Ok(path) => Decision::File(path),
        Err(e) => Decision::Error(e),
    }
}

/// Resolution as seen from user code or a vendored package: vendored roots
/// first, then the store pool, then builtins last (bare specifiers).
fn app_vendor_resolve(
    store: Option<&Path>,
    vendor: Option<&Path>,
    _referrer: &str,
    specifier: &str,
) -> Decision {
    // ---- explicit npm:/jsr: pins: vendored (version-checked) then store ------
    if specifier.starts_with("npm:") || specifier.starts_with("jsr:") {
        if let Some(v) = vendor {
            match vendor_pinned_lookup(v, specifier) {
                Ok(Some(path)) => return Decision::File(path),
                Ok(None) => {} // not vendored at the pinned version -> store below
                Err(e) => return Decision::Error(e),
            }
        }
        let Some(s) = store else {
            return Decision::Error(
                "this runtime has no package store configured (INKA_STORE is unset); \
                 run `inka update` to install vendored packages"
                    .into(),
            );
        };
        return match store_lookup(s, specifier) {
            Ok(path) => Decision::File(path),
            Err(e) => Decision::Error(e),
        };
    }
    // ---- schemes (file:/node:/data:/…) and remote imports ----------
    if has_scheme(specifier) {
        if specifier.starts_with("http://") || specifier.starts_with("https://") {
            return Decision::Error(format!(
                "network module imports are disabled ('{specifier}'); \
                 vendor the package with `inka add` instead"
            ));
        }
        return Decision::UseDefault;
    }
    // ---- no scheme: relative or bare ----
    if specifier.starts_with("./") || specifier.starts_with("../") || specifier.starts_with('/') {
        return Decision::UseDefault;
    }
    // ---- bare package name: vendored -> store -> builtin ----
    let (name, _sub) = split_bare(specifier);
    for ident in bare_store_identities(&name) {
        if let Some(v) = vendor {
            let pkg_root = v.join(&ident);
            // A vendored entry is a package only when it is a real root (has a
            // package.json); a stray dir must be skipped so the lookup order
            // (store -> builtin -> error) continues, not surfaced as a bogus
            // "cannot read package.json" failure.
            if pkg_root.join("package.json").is_file() {
                let sub = split_bare(specifier).1;
                return match resolve_pkg_file(&pkg_root, sub.as_deref()) {
                    Ok(path) => Decision::File(path),
                    Err(e) => Decision::Error(e),
                };
            }
        }
    }
    if let Some(s) = store {
        // Only consult the store when the package is actually installed there;
        // a found-but-unservable entry (e.g. CommonJS) must surface its error
        // rather than being mistaken for an absent package.
        let present = bare_store_identities(&name)
            .iter()
            .any(|id| store_package_dir(s, id).is_dir());
        if present {
            return match store_bare_top(s, specifier) {
                Ok(path) => Decision::File(path),
                Err(e) => Decision::Error(e),
            };
        }
    }
    if let Some(node_spec) = node_builtin_spec(specifier) {
        return Decision::Builtin(node_spec);
    }
    match (vendor.is_some(), store.is_some()) {
        (false, false) => Decision::Error(format!(
            "bare import '{specifier}' cannot be resolved: no package store configured \
             (INKA_STORE is unset); run `inka update` to install it"
        )),
        (_, true) => Decision::Error(format!(
            "package '{name}' is not in the package store; run `inka update` to install it \
             (or `inka add {name}` to vendor it for this project)"
        )),
        (true, false) => Decision::Error(format!(
            "cannot resolve bare import '{specifier}' (not vendored and no default store); \
             run `inka add {name}` to vendor it"
        )),
    }
}

/// If `spec` (npm:/jsr: pin) names a vendored package at the pinned version,
/// return its entry file; Ok(None) when it isn't vendored at that version.
fn vendor_pinned_lookup(vendor: &Path, spec: &str) -> Result<Option<PathBuf>, String> {
    let ps = parse_pkg_specifier(spec)?;
    let pkg_root = vendor.join(&ps.name);
    // Only a real package root (with a package.json) can satisfy a pin; a stray
    // dir falls through to the store instead of a confusing "no readable version".
    if !pkg_root.join("package.json").is_file() {
        return Ok(None);
    }
    if let Some(req) = ps.req.as_deref() {
        let Some(installed) = installed_version(&pkg_root) else {
            return Err(format!("vendored package '{}' has no readable version", ps.name));
        };
        if !version_satisfies(&installed, req) {
            return Ok(None); // pinned to a version we don't have vendored
        }
    }
    resolve_pkg_file(&pkg_root, ps.sub.as_deref()).map(Some)
}

fn has_scheme(spec: &str) -> bool {
    spec.split_once(':').is_some()
}

fn referrer_file_path(referrer: &str) -> Option<PathBuf> {
    url::Url::parse(referrer)
        .ok()
        .and_then(|u| u.to_file_path().ok())
}

/// Bare specifiers that map to Node built-ins (Node semantics: core wins).
fn node_builtin_spec(spec: &str) -> Option<String> {
    const SIMPLE: &[&str] = &[
        "assert", "async_hooks", "buffer", "child_process", "cluster", "console",
        "constants", "crypto", "dgram", "diagnostics_channel", "dns", "domain",
        "events", "fs", "http", "http2", "https", "inspector", "module", "net",
        "os", "path", "perf_hooks", "process", "punycode", "querystring",
        "readline", "repl", "stream", "string_decoder", "sys", "timers", "tls",
        "trace_events", "tty", "url", "util", "v8", "vm", "wasi",
        "worker_threads", "zlib",
    ];
    const SUB: &[(&str, &str)] = &[
        ("assert/strict", "node:assert/strict"),
        ("dns/promises", "node:dns/promises"),
        ("fs/promises", "node:fs/promises"),
        ("path/posix", "node:path/posix"),
        ("path/win32", "node:path/win32"),
        ("readline/promises", "node:readline/promises"),
        ("stream/consumers", "node:stream/consumers"),
        ("stream/promises", "node:stream/promises"),
        ("stream/web", "node:stream/web"),
        ("timers/promises", "node:timers/promises"),
        ("util/types", "node:util/types"),
    ];
    for (key, node) in SUB {
        if *key == spec {
            return Some((*node).to_string());
        }
    }
    if SIMPLE.contains(&spec) {
        return Some(format!("node:{spec}"));
    }
    None
}

/// One parsed `npm:`/`jsr:` specifier, normalized to its npm identity.
struct PkgSpec {
    /// npm package name, e.g. `zod` or `@jsr/std__assert`.
    name: String,
    /// Optional version requirement text as written (no leading `@`).
    req: Option<String>,
    /// Optional subpath (no leading `/`).
    sub: Option<String>,
}

fn parse_pkg_specifier(spec: &str) -> Result<PkgSpec, String> {
    let body = if let Some(rest) = spec.strip_prefix("npm:") {
        rest.to_string()
    } else if let Some(rest) = spec.strip_prefix("jsr:") {
        // jsr:@scope/name -> npm @jsr/scope__name (jsr's npm-compatibility mirror).
        let rest = rest.trim();
        let (scope, after) = rest.split_once('/').ok_or_else(|| {
            format!("invalid jsr specifier '{spec}' (expected jsr:@scope/name[...])")
        })?;
        let scope = scope.strip_prefix('@').unwrap_or(scope);
        let (name, tail) = split_name_suffix(after);
        format!("@jsr/{scope}__{name}{tail}")
    } else {
        return Err(format!("not a package specifier: '{spec}'"));
    };
    parse_npm_body(&body)
}

/// Splits `name` from the rest of a package body (`name[@req][/sub]`).
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
    let mut req = None;
    let mut sub = None;
    if let Some(tail) = rest.strip_prefix('@') {
        let (r, s) = match tail.split_once('/') {
            Some((r, s)) => (r, Some(s.to_string())),
            None => (tail, None),
        };
        req = Some(r.to_string());
        sub = s;
    } else if let Some(s) = rest.strip_prefix('/') {
        sub = Some(s.to_string());
    }
    Ok(PkgSpec { name, req, sub })
}

fn version_satisfies(v: &Version, req: &str) -> bool {
    let req = req.trim();
    if let Ok(exact) = Version::parse_standard(req) {
        return v == &exact;
    }
    match VersionReq::parse_from_npm(req) {
        Ok(vr) => vr.tag().is_none() && vr.matches(v),
        Err(_) => false,
    }
}

/// Render an npm identity for user-facing messages.
fn npm_display(npm_name: &str) -> String {
    if let Some(rest) = npm_name.strip_prefix("@jsr/") {
        if let Some((scope, pkg)) = rest.split_once("__") {
            return format!("jsr:@{scope}/{pkg}");
        }
    }
    npm_name.to_string()
}

fn store_package_dir(store: &Path, npm_name: &str) -> PathBuf {
    store.join("node_modules").join(npm_name)
}

/// Reads the installed version of a package root from its package.json.
fn installed_version(pkg_root: &Path) -> Option<Version> {
    let raw = std::fs::read(pkg_root.join("package.json")).ok()?;
    let v: Value = serde_json::from_slice(&raw).ok()?;
    let text = v.get("version").and_then(Value::as_str)?;
    Version::parse_standard(text).ok()
}

fn resolve_store_package(
    store: &Path,
    npm_name: &str,
    req: Option<&str>,
    sub: Option<&str>,
) -> Result<PathBuf, String> {
    let pkg_root = store_package_dir(store, npm_name);
    if !pkg_root.is_dir() {
        return Err(format!(
            "package '{}' is not in the package store; run `inka update` to install it",
            npm_display(npm_name)
        ));
    }
    if let Some(installed) = installed_version(&pkg_root) {
        if let Some(r) = req.map(str::trim).filter(|s| !s.is_empty()) {
            if !version_satisfies(&installed, r) {
                return Err(format!(
                    "package '{}' is installed at {installed}, which does not satisfy '{r}'; \
                     run `inka update` to install the requested version",
                    npm_display(npm_name)
                ));
            }
        }
    }
    resolve_pkg_file(&pkg_root, sub)
}

fn store_lookup(store: &Path, spec: &str) -> Result<PathBuf, String> {
    let ps = parse_pkg_specifier(spec)?;
    resolve_store_package(store, &ps.name, ps.req.as_deref(), ps.sub.as_deref())
}

fn bare_store_identities(name: &str) -> Vec<String> {
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

fn store_bare_top(store: &Path, spec: &str) -> Result<PathBuf, String> {
    let (name, sub) = split_bare(spec);
    for ident in bare_store_identities(&name) {
        if store_package_dir(store, &ident).is_dir() {
            return resolve_store_package(store, &ident, None, sub.as_deref());
        }
    }
    Err(format!(
        "package '{name}' is not in the package store; run `inka update` to install it"
    ))
}

/// ESM-ness of a resolved store file. A `.js` file selected through the `import`
/// or `node` condition of an `exports` map is ESM regardless of the package's
/// `"type"` (the dual-package dist/esm pattern), so we must know the selection
/// context, not just the extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EsmContext {
    /// Selected via the `import`/`node` condition: ESM by context.
    ByCondition,
    /// Selected via a plain-string/`default` target or legacy `main`: classify
    /// by file extension + package `"type"`.
    Classify,
}

/// CommonJS message: the engine is ESM-only and cannot run `require`/CJS files.
fn cjs_error(pkg_name: &str, file: &Path) -> String {
    format!(
        "'{}' (package '{pkg_name}') is CommonJS, which this engine cannot run; \
         vendor the patched ESM store (`inka pkg snapshot` applies its `patches/`) \
         or use an ESM alternative",
        file.display()
    )
}

/// Reject files the engine cannot serve: CommonJS (and anything not ESM-typed).
/// `.cjs` is rejected even when reached via an `import` condition; `.mjs`/`.json`
/// are always fine; a `.js`/other file is fine only when ESM-by-condition or the
/// package declares `"type":"module"`.
fn ensure_esm(pkg: &Value, file: &Path, ctx: EsmContext) -> Result<(), String> {
    let name = pkg
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    match file.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "cjs" => Err(cjs_error(name, file)),
        "mjs" | "json" => Ok(()),
        _ => {
            if ctx == EsmContext::ByCondition {
                return Ok(());
            }
            let is_module = pkg.get("type").and_then(Value::as_str) == Some("module");
            if is_module {
                Ok(())
            } else {
                Err(cjs_error(name, file))
            }
        }
    }
}

fn exports_target(exports: &Value, subpath: &str) -> Result<(String, EsmContext), String> {
    fn pick_conditions(v: &Value) -> Result<(String, EsmContext), String> {
        match v {
            Value::String(s) => Ok((s.clone(), EsmContext::Classify)),
            Value::Array(items) => {
                for item in items {
                    if let Ok(x) = pick_conditions(item) {
                        return Ok(x);
                    }
                }
                Err("no usable export target".to_string())
            }
            Value::Object(map) => {
                // Prefer the first usable ESM condition in a fixed priority
                // (import > node > default) regardless of key ordering — some
                // serializers sort keys, and we never want `default` (often the
                // CJS build) to beat an explicit `import` target.
                for cond in EXPORT_CONDITIONS {
                    if let Some(val) = map.get(cond) {
                        let (target, _) = pick_conditions(val)?;
                        let ctx = if cond == "import" || cond == "node" {
                            EsmContext::ByCondition
                        } else {
                            EsmContext::Classify
                        };
                        return Ok((target, ctx));
                    }
                }
                Err(
                    "package has no import/default export target \
                     (CommonJS-only packages are not supported)"
                        .to_string(),
                )
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
                        let (t, ctx) = pick_conditions(v)?;
                        hit = Some((t.replace('*', rem), ctx));
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

fn legacy_package_target(pkg_root: &Path, pkg: &Value, sub: &str) -> Result<String, String> {
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
        if let Some(main) = pkg.get("main").and_then(Value::as_str) {
            if let Some(t) = resolve_loose(main) {
                return Ok(t);
            }
        }
        return resolve_loose("index.js").ok_or_else(|| "package has no main entry".to_string());
    }
    resolve_loose(sub).ok_or_else(|| format!("cannot resolve file '{sub}' in package"))
}

fn resolve_pkg_file(pkg_root: &Path, subpath: Option<&str>) -> Result<PathBuf, String> {
    let pkg_json_path = pkg_root.join("package.json");
    let raw = std::fs::read_to_string(&pkg_json_path)
        .map_err(|e| format!("cannot read {}: {e}", pkg_json_path.display()))?;
    let pkg: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("invalid package.json in {}: {e}", pkg_json_path.display()))?;
    let sub = subpath.unwrap_or("").trim_start_matches("./");

    let (target, ctx) = match pkg.get("exports") {
        Some(Value::Null) | None => (
            legacy_package_target(pkg_root, &pkg, sub)?,
            EsmContext::Classify,
        ),
        Some(exports) => exports_target(exports, sub)?,
    };
    let target = target.trim_start_matches("./");
    let file = pkg_root.join(target);
    if target.split('/').any(|c| c == "..")
        || Path::new(target)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!("export target '{target}' escapes the package directory"));
    }
    if !file.is_file() {
        return Err(format!(
            "store package module not found on disk: {}",
            file.display()
        ));
    }
    ensure_esm(&pkg, &file, ctx)?;
    Ok(file)
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

fn store_bare_lookup(store: &Path, spec: &str, referrer: &Path) -> Result<PathBuf, String> {
    let (name, sub) = split_bare(spec);
    let mut dir = referrer
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| store.to_path_buf());
    loop {
        let cand = dir.join("node_modules").join(&name);
        if cand.is_dir() {
            return resolve_pkg_file(&cand, sub.as_deref());
        }
        let Some(parent) = dir.parent() else { break };
        if !parent.starts_with(store) {
            break;
        }
        dir = parent.to_path_buf();
    }
    Err(format!(
        "cannot resolve '{spec}' from the store (not an installed dependency); \
         run `inka update` to install it"
    ))
}

// ---- C ABI ---------------------------------------------------------------

const KIND_USE_DEFAULT: c_int = 0;
const KIND_FILE: c_int = 1;
const KIND_BUILTIN: c_int = 2;
const KIND_ERROR: c_int = 3;

fn to_c(payload: &str) -> *mut c_char {
    CString::new(payload)
        .map(CString::into_raw)
        .unwrap_or_else(|_| CString::new("error").unwrap().into_raw())
}

#[no_mangle]
pub extern "C" fn inka_resolver_version() -> *const c_char {
    static V: std::sync::OnceLock<CString> = std::sync::OnceLock::new();
    V.get_or_init(|| CString::new(env!("CARGO_PKG_VERSION")).expect("nul")).as_ptr()
}

#[no_mangle]
pub extern "C" fn inka_resolver_abi() -> c_int {
    2
}

/// Resolve `specifier` (imported from `referrer`) against the vendored roots
/// (`vendor`) and the store pool (`store`).
///
/// `store` / `vendor` may be "" (not configured). On return `*a` (and `*b`,
/// unused) may hold an owned C string that must be released with
/// `inka_resolver_free`. Returns a `KIND_*` code; for KIND_FILE `*a` is the
/// absolute file path; for KIND_BUILTIN `*a` is a full `node:<name>` specifier;
/// for KIND_ERROR `*a` is the message.
#[no_mangle]
pub unsafe extern "C" fn inka_resolver_resolve(
    store: *const c_char,
    vendor: *const c_char,
    referrer: *const c_char,
    specifier: *const c_char,
    a: *mut *mut c_char,
    _b: *mut *mut c_char,
) -> c_int {
    if a.is_null() {
        return KIND_ERROR;
    }
    *a = std::ptr::null_mut();
    let store = match opt_str(store) {
        s if s.is_empty() => None,
        s => Some(PathBuf::from(s)),
    };
    let vendor = match opt_str(vendor) {
        s if s.is_empty() => None,
        s => Some(PathBuf::from(s)),
    };
    let referrer = opt_str(referrer);
    let specifier = opt_str(specifier);
    let decision = resolve_v2(store.as_deref(), vendor.as_deref(), &referrer, &specifier);
    match decision {
        Decision::UseDefault => KIND_USE_DEFAULT,
        Decision::File(p) => {
            *a = to_c(&p.to_string_lossy());
            KIND_FILE
        }
        Decision::Builtin(s) => {
            *a = to_c(&s);
            KIND_BUILTIN
        }
        Decision::Error(e) => {
            *a = to_c(&e);
            KIND_ERROR
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn inka_resolver_free(p: *mut c_char) {
    if !p.is_null() {
        drop(CString::from_raw(p));
    }
}

unsafe fn opt_str(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    CStr::from_ptr(p).to_string_lossy().into_owned()
}

// ---- tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn write_pkg(root: &Path, name: &str, version: &str, entry: &str) {
        let dir = root.join("node_modules").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let exports = serde_json::json!({
            ".": { "types": "./index.d.ts", "import": entry, "default": entry }
        });
        std::fs::write(
            dir.join("package.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "name": name,
                "version": version,
                "exports": exports,
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join(entry), "export const x = 1;\n").unwrap();
    }

    #[test]
    fn classify_defaults() {
        let d = resolve(None, "file:///a/main.ts", "./util.ts");
        assert_eq!(d, Decision::UseDefault);
        let d = resolve(None, "file:///a/main.ts", "node:vm");
        assert_eq!(d, Decision::UseDefault);
        let d = resolve(None, "file:///a/main.ts", "file:///x.js");
        assert_eq!(d, Decision::UseDefault);
    }

    #[test]
    fn builtins_http_and_bare() {
        let d = resolve(None, "file:///a/main.ts", "vm");
        assert_eq!(d, Decision::Builtin("node:vm".into()));
        let d = resolve(None, "file:///a/main.ts", "fs/promises");
        assert_eq!(d, Decision::Builtin("node:fs/promises".into()));
        let d = resolve(None, "file:///a/main.ts", "https://x/y.js");
        assert!(matches!(d, Decision::Error(m) if m.contains("network module imports are disabled")));
        let d = resolve(None, "file:///a/main.ts", "nosuchpkg");
        assert!(matches!(d, Decision::Error(m) if m.contains("no package store configured")));
    }

    #[test]
    fn prefers_import_condition_over_default() {
        let tmp = std::env::temp_dir().join(format!("inkares-ord-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let dir = tmp.join("node_modules").join("effect");
        std::fs::create_dir_all(&dir).unwrap();
        // keys deliberately out of ESM order to catch sorted-key handling
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"effect","version":"3.22.1","exports":{".":{"types":"./index.d.ts","default":"./dist/cjs/index.js","import":"./dist/esm/index.js"}}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("dist/esm")).unwrap();
        std::fs::create_dir_all(dir.join("dist/cjs")).unwrap();
        std::fs::write(dir.join("dist/esm/index.js"), "export const Effect = 1;\n").unwrap();
        std::fs::write(dir.join("dist/cjs/index.js"), "module.exports = {};\n").unwrap();
        let d = resolve(Some(&tmp), "file:///a/main.ts", "effect");
        assert!(
            matches!(&d, Decision::File(p) if p.ends_with("effect/dist/esm/index.js")),
            "expected the ESM import build, got {d:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn store_file_and_version() {
        let tmp = std::env::temp_dir().join(format!("inkares-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        write_pkg(&tmp, "zod", "3.23.0", "./index.js");
        let d = resolve(Some(&tmp), "file:///a/main.ts", "zod");
        assert!(matches!(&d, Decision::File(p) if p.ends_with("zod/index.js")), "{d:?}");
        let d = resolve(Some(&tmp), "file:///a/main.ts", "npm:zod@3.23.0");
        assert!(matches!(d, Decision::File(_)));
        let d = resolve(Some(&tmp), "file:///a/main.ts", "npm:zod@9.9.9");
        assert!(matches!(d, Decision::Error(m) if m.contains("does not satisfy")));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Seed a package with the given package.json + files; resolve its root.
    fn seed_pkg(tmp: &Path, name: &str, pkg_json: serde_json::Value, files: &[(&str, &str)]) {
        let dir = tmp.join("node_modules").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            serde_json::to_string_pretty(&pkg_json).unwrap(),
        )
        .unwrap();
        for (rel, body) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
    }

    fn fresh_store() -> std::path::PathBuf {
        scratch_dir("store")
    }

    /// A unique scratch dir per call. The resolver tests share this process's
    /// temp namespace, so a fixed per-pid dir would be raced by the parallel
    /// test harness (one test's remove_dir_all wipes another's fixtures).
    fn scratch_dir(kind: &str) -> std::path::PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "inkares-{kind}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn rejects_legacy_cjs_main() {
        let tmp = fresh_store();
        seed_pkg(
            &tmp,
            "legacycjs",
            serde_json::json!({ "name": "legacycjs", "version": "1.0.0", "main": "index.js" }),
            &[("index.js", "module.exports = {};\n")],
        );
        let d = resolve(Some(&tmp), "file:///a/main.ts", "legacycjs");
        assert!(
            matches!(&d, Decision::Error(m) if m.contains("CommonJS")),
            "got {d:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rejects_default_only_cjs_build_even_with_type_missing() {
        let tmp = fresh_store();
        // exports object is NOT a subpath map: only a `default` (CJS) target.
        seed_pkg(
            &tmp,
            "cjsonly",
            serde_json::json!({
                "name": "cjsonly", "version": "1.0.0",
                "exports": { "default": "./index.cjs" }
            }),
            &[("index.cjs", "module.exports = {};\n")],
        );
        let d = resolve(Some(&tmp), "file:///a/main.ts", "cjsonly");
        assert!(
            matches!(&d, Decision::Error(m) if m.contains("CommonJS")),
            "got {d:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn allows_type_module_legacy_and_patched_shape() {
        let tmp = fresh_store();
        // legacy ESM via "type":"module" + main index.js
        seed_pkg(
            &tmp,
            "esmlegacy",
            serde_json::json!({
                "name": "esmlegacy", "version": "1.0.0", "type": "module", "main": "index.js"
            }),
            &[("index.js", "export const x = 1;\n")],
        );
        let d = resolve(Some(&tmp), "file:///a/main.ts", "esmlegacy");
        assert!(
            matches!(&d, Decision::File(p) if p.ends_with("esmlegacy/index.js")),
            "got {d:?}"
        );
        // patched-ws shape: exports "." -> import/default ./esm.js, no "type"
        seed_pkg(
            &tmp,
            "wspatched",
            serde_json::json!({
                "name": "wspatched", "version": "8.21.3",
                "exports": {
                    "./package.json": "./package.json",
                    ".": { "import": "./esm.js", "default": "./esm.js" }
                }
            }),
            &[("esm.js", "export const WebSocket = 1;\n")],
        );
        let d = resolve(Some(&tmp), "file:///a/main.ts", "wspatched");
        assert!(
            matches!(&d, Decision::File(p) if p.ends_with("wspatched/esm.js")),
            "got {d:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Build a vendored root (dir containing package roots by name) in tmp/vend.
    fn seed_vendor(root: &Path, pkg: serde_json::Value, files: &[(&str, &str)]) {
        let name = pkg.get("name").and_then(serde_json::Value::as_str).unwrap();
        let dir = root.join("vend").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("package.json"), serde_json::to_string_pretty(&pkg).unwrap())
            .unwrap();
        for (rel, body) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
    }

    fn fresh_vendor() -> std::path::PathBuf {
        let tmp = scratch_dir("vnd");
        let v = tmp.join("vend");
        std::fs::create_dir_all(&v).unwrap();
        v
    }

    #[test]
    fn app_tier_serves_vendored_and_shadows_store() {
        let tmp = fresh_store();
        let v = fresh_vendor();
        let vendored = tmp.join("vend");
        let _ = &v;
        // vendored widget (version 9) and a store widget (version 1)
        seed_vendor(
            &tmp,
            serde_json::json!({ "name": "widget", "version": "9.0.0", "type": "module", "main": "index.js" }),
            &[("index.js", "export const fromVendor = true;\n")],
        );
        // seed_vendor writes under root/vend/<name>; alias the vendor root used below.
        seed_pkg(
            &tmp,
            "widget",
            serde_json::json!({ "name": "widget", "version": "1.0.0", "type": "module", "main": "index.js" }),
            &[("index.js", "export const fromStore = true;\n")],
        );
        let store_root = &tmp;
        let vendor_root = &tmp.join("vend");
        let d = resolve_v2(
            Some(store_root),
            Some(vendor_root),
            "file:///app/main.ts",
            "widget",
        );
        assert!(
            matches!(&d, Decision::File(p) if p.starts_with(&tmp.join("vend/widget"))),
            "expected the vendored copy for app code, got {d:?}"
        );
        // pinned to a vendored version is served from the vendor too
        let d = resolve_v2(
            Some(store_root),
            Some(vendor_root),
            "file:///app/main.ts",
            "npm:widget@9.0.0",
        );
        assert!(matches!(&d, Decision::File(p) if p.starts_with(&tmp.join("vend/widget"))));
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = vendored;
    }

    #[test]
    fn store_referrer_ignores_vendor() {
        let tmp = fresh_store();
        // store copy of widget
        seed_pkg(
            &tmp,
            "widget",
            serde_json::json!({ "name": "widget", "version": "1.0.0", "type": "module", "main": "index.js" }),
            &[("index.js", "export const fromStore = true;\n")],
        );
        // also a vendor copy
        seed_vendor(
            &tmp,
            serde_json::json!({ "name": "widget", "version": "9.0.0", "type": "module", "main": "index.js" }),
            &[("index.js", "export const fromVendor = true;\n")],
        );
        let store_root = &tmp;
        let vendor_root = &tmp.join("vend");
        // a store-internal referrer must resolve the STORE copy, never vendored/
        let referrer = format!("file://{}", store_root.join("node_modules/widget/index.js").display());
        let d = resolve_v2(
            Some(store_root),
            Some(vendor_root),
            &referrer,
            "widget",
        );
        assert!(
            matches!(&d, Decision::File(p) if p.starts_with(&store_root.join("node_modules/widget"))),
            "store-internal import must not see vendored, got {d:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn app_tier_builtin_fallback_and_no_store() {
        // builtin still reachable when nothing vendored/store provides the name
        let v = fresh_vendor();
        let d = resolve_v2(None, Some(&v), "file:///app/main.ts", "vm");
        assert_eq!(d, Decision::Builtin("node:vm".into()));
        // unknown bare name with vendor only -> clear error
        let d = resolve_v2(None, Some(&v), "file:///app/main.ts", "nosuchpkg");
        assert!(matches!(&d, Decision::Error(m) if m.contains("nosuchpkg")));
        let _ = std::fs::remove_dir_all(v.parent().unwrap());
    }

    #[test]
    fn pinned_mismatch_falls_back_to_store_or_errors() {
        let tmp = fresh_store();
        let v = fresh_vendor();
        // vendored widget only at 9.0.0
        seed_vendor(
            &tmp,
            serde_json::json!({ "name": "widget", "version": "9.0.0", "type": "module", "main": "index.js" }),
            &[("index.js", "export const v = 9;\n")],
        );
        let vendor_root = &tmp.join("vend");
        // pinned to a version we don't have vendored, and no store configured
        let d = resolve_v2(None, Some(vendor_root), "file:///app/main.ts", "npm:widget@1.0.0");
        assert!(matches!(&d, Decision::Error(_)), "got {d:?}");
        // pin matching the vendored version resolves from vendor
        let d = resolve_v2(None, Some(vendor_root), "file:///app/main.ts", "npm:widget@9.0.0");
        assert!(matches!(&d, Decision::File(p) if p.starts_with(&tmp.join("vend/widget"))));
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = v;
    }

    // A stray dir under vendored/ without a package.json is NOT a package: a
    // bare import must fall through (store -> builtin -> clean error) instead
    // of surfacing a bogus "cannot read …/package.json" failure.
    #[test]
    fn junk_vendor_dir_without_package_json_is_skipped() {
        let v = fresh_vendor();
        std::fs::create_dir_all(v.join("junk")).unwrap();
        std::fs::write(v.join("junk").join("file.txt"), "stray\n").unwrap();
        let d = resolve_v2(None, Some(&v), "file:///app/main.ts", "junk");
        assert!(
            matches!(&d, Decision::Error(m)
                if m.contains("cannot resolve bare import 'junk'")
                    && !m.contains("package.json")
                    && !m.contains("cannot read")),
            "got {d:?}"
        );
        let _ = std::fs::remove_dir_all(v.parent().unwrap());
    }

    // The same guard applies to pinned (npm:/jsr:) lookups: a stray dir falls
    // through to the store instead of a confusing "no readable version" error.
    #[test]
    fn vendor_pinned_stray_dir_falls_through() {
        let v = fresh_vendor();
        std::fs::create_dir_all(v.join("widget")).unwrap();
        std::fs::write(v.join("widget").join("index.js"), "stray\n").unwrap();
        let d = resolve_v2(None, Some(&v), "file:///app/main.ts", "npm:widget@1.0.0");
        assert!(
            matches!(&d, Decision::Error(m) if !m.contains("no readable version")),
            "got {d:?}"
        );
        let _ = std::fs::remove_dir_all(v.parent().unwrap());
    }
}

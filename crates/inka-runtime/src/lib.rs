// inka runtime: a cdylib embedding the Deno runtime (deno_runtime crate)
// behind the frozen inka C ABI.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::OnceLock;

use deno_runtime::deno_core::url::Url;
use deno_runtime::deno_core::{
    ModuleCodeString, ModuleLoadOptions, ModuleLoadResponse, ModuleLoader, ModuleName,
    ModuleResolveResponse, ModuleSource, ModuleSourceCode, ModuleSpecifier, ModuleType,
    RequestedModuleType,
};
use deno_runtime::deno_fetch::dns::Resolver as FetchDnsResolver;
use deno_runtime::deno_fs::{FileSystem, RealFs};
use deno_runtime::deno_permissions::{
    PermissionDescriptorParser, Permissions, PermissionsContainer, PermissionsOptions,
    RuntimePermissionDescriptorParser,
};
use deno_runtime::deno_web::{BlobStore, InMemoryBroadcastChannel};
use deno_runtime::worker::{MainWorker, WorkerOptions, WorkerServiceOptions};
use deno_runtime::transpile::maybe_transpile_source;
use deno_runtime::{FeatureChecker, WorkerLogLevel};

use node_resolver::errors;
use node_resolver::{InNpmPackageChecker, NpmPackageFolderResolver, UrlOrPathRef};
use deno_error::JsErrorBox;
use deno_semver::{Version, VersionReq};
use serde_json::Value;
use sys_traits::impls::RealSys;

const DENO_RUNTIME_VERSION: &str = "0.266.0";

static STARTUP_SNAPSHOT: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/CLI_SNAPSHOT.bin"));

mod runtime_snapshot {
    include!(concat!(env!("OUT_DIR"), "/EXTENSION_RESIDUAL_SOURCES.rs"));
}

const STORE_PACKAGES_DIR: &str = "packages";

/// Allow-listed condition keys for package.json `exports` target selection.
/// `types` and `require` (CommonJS) are deliberately skipped; only ESM-capable
/// targets are used.
const EXPORT_CONDITIONS: [&str; 3] = ["import", "node", "default"];

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

/// Store-backed module loader. Serves:
///   - the artifact tree (or the staged single-entry tree) — local files,
///   - vendored `npm:`/`jsr:` packages from a global self-contained store,
///   - `node:`/`data:`/`file:` built-ins (as before),
/// and rejects network imports outright. Reading is confined to the artifact
/// tree and the store; nothing outside those roots is ever served.
struct PkgLoader {
    /// Root of the artifact tree (or the staged temp tree for single-file runs).
    artifact_root: PathBuf,
    /// Root of the global package store (`<store>/packages/<name>/<version>/…`).
    store_root: Option<PathBuf>,
}

fn store_root_env() -> Option<PathBuf> {
    std::env::var_os("INKA_STORE").map(PathBuf::from)
}

fn has_scheme(spec: &str) -> bool {
    spec.split_once(':').is_some()
}

fn is_bare(spec: &str) -> bool {
    !has_scheme(spec) && !spec.starts_with("./") && !spec.starts_with("../") && !spec.starts_with('/')
}

fn referrer_file_path(referrer: &str) -> Option<PathBuf> {
    Url::parse(referrer).ok().and_then(|u| u.to_file_path().ok())
}

/// Lists the installed versions of a package: `Vec<(dir_name, version)>`
/// sorted ascending. A missing dir simply means "nothing installed".
fn store_package_versions(dir: &Path) -> Result<Vec<(String, Version)>, String> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(out),
    };
    for ent in entries.flatten() {
        if !ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = ent.file_name().to_string_lossy().into_owned();
        if let Ok(v) = Version::parse_standard(&name) {
            out.push((name, v));
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(out)
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

/// Chooses the installed version dir for a package given the requested version.
fn pick_package_version(
    name: &str,
    versions: &[(String, Version)],
    req: Option<&str>,
) -> Result<String, String> {
    let avail: Vec<&str> = versions.iter().map(|(n, _)| n.as_str()).collect();
    if versions.is_empty() {
        return Err(format!(
            "package '{name}' is not in the package store (nothing installed); \
             run `inka pkg seed` to install it"
        ));
    }
    let req = req.map(str::trim).filter(|s| !s.is_empty());
    let matching: Vec<&(String, Version)> = match &req {
        None => versions.iter().collect(),
        Some(r) => versions
            .iter()
            .filter(|(_, v)| version_satisfies(v, r))
            .collect(),
    };
    match req {
        None => {
            if matching.len() == 1 {
                Ok(matching[0].0.clone())
            } else {
                Err(format!(
                    "multiple versions of '{name}' are installed ({avail:?}); \
                     import an exact version (e.g. npm:{name}@<version>)"
                ))
            }
        }
        Some(r) => {
            if matching.is_empty() {
                Err(format!(
                    "no installed version of '{name}' satisfies '{r}' (have {avail:?}); \
                     run `inka pkg seed` to install it"
                ))
            } else {
                // Prefer the highest satisfying installed version.
                Ok(matching
                    .iter()
                    .max_by(|a, b| a.1.cmp(&b.1))
                    .map(|m| m.0.clone())
                    .expect("matching is non-empty"))
            }
        }
    }
}

/// Picks the importable JS target out of a `package.json` `exports` value for
/// the requested subpath. Returns a package-relative path (may start with `./`).
fn exports_target(exports: &Value, subpath: &str) -> Result<String, String> {
    fn pick_conditions(v: &Value) -> Result<String, String> {
        match v {
            Value::String(s) => Ok(s.clone()),
            Value::Array(items) => {
                for item in items {
                    if let Ok(s) = pick_conditions(item) {
                        return Ok(s);
                    }
                }
                Err("no usable export target".to_string())
            }
            Value::Object(map) => {
                for (k, val) in map {
                    if EXPORT_CONDITIONS.contains(&k.as_str()) {
                        return pick_conditions(val);
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
            // support pattern keys like "./locales/*"
            let mut hit = None;
            for (k, v) in map {
                if let Some(star) = k.strip_suffix('*') {
                    let prefix = star.strip_prefix("./").unwrap_or(star);
                    if let Some(rem) = sub.strip_prefix(prefix) {
                        let t = pick_conditions(v)?;
                        hit = Some(t.replace('*', rem));
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
        // exports applies to the package root only.
        pick_conditions(exports)?
    } else {
        return Err(format!("no exported subpath './{sub}' for this package"));
    };
    Ok(target)
}

/// Legacy (no `exports`) target resolution: `main` or file/index lookup.
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

/// Resolves a subpath ("" = package root) inside a package directory to a
/// concrete on-disk file, honoring `exports` with a legacy fallback.
fn resolve_pkg_file(pkg_root: &Path, subpath: Option<&str>) -> Result<PathBuf, String> {
    let pkg_json_path = pkg_root.join("package.json");
    let raw = std::fs::read_to_string(&pkg_json_path)
        .map_err(|e| format!("cannot read {}: {e}", pkg_json_path.display()))?;
    let pkg: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("invalid package.json in {}: {e}", pkg_json_path.display()))?;
    let sub = subpath.unwrap_or("").trim_start_matches("./");

    let target = match pkg.get("exports") {
        Some(Value::Null) | None => legacy_package_target(pkg_root, &pkg, sub)?,
        Some(exports) => exports_target(exports, sub)?,
    };
    let target = target.trim_start_matches("./");
    let file = pkg_root.join(target);
    // never allow an export target to walk out of the package directory
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
    Ok(file)
}

/// Resolves an `npm:`/`jsr:` specifier against the store to a concrete file.
fn store_lookup(store: &Path, spec: &str) -> Result<PathBuf, String> {
    let ps = parse_pkg_specifier(spec)?;
    let packages = store.join(STORE_PACKAGES_DIR);
    let base = packages.join(&ps.name);
    if !base.starts_with(&packages) {
        return Err(format!("unsafe package name in '{spec}'"));
    }
    let versions = store_package_versions(&base)?;
    let dir_name = pick_package_version(&ps.name, &versions, ps.req.as_deref())?;
    let pkg_dir = base.join(&dir_name);
    let pkg_root = pkg_dir.join("node_modules").join(&ps.name);
    if !pkg_root.is_dir() {
        return Err(format!(
            "store package {}@{} is missing its node_modules/{} tree; re-run `inka pkg seed`",
            ps.name, dir_name, ps.name
        ));
    }
    resolve_pkg_file(&pkg_root, ps.sub.as_deref())
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

/// Node-style resolution of a bare specifier originating inside the store
/// (a vendored package importing one of its installed dependencies).
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
         run `inka pkg seed` to install it"
    ))
}

fn file_url_response(path: &Path) -> ModuleResolveResponse {
    match ModuleSpecifier::from_file_path(path) {
        Ok(u) => Ok(u),
        Err(_) => Err(JsErrorBox::generic(format!(
            "cannot form a file URL for {}",
            path.display()
        ))),
    }
}

impl ModuleLoader for PkgLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: deno_core::ResolutionKind,
    ) -> ModuleResolveResponse {
        // Vendored package specifiers resolve from the store.
        if specifier.starts_with("npm:") || specifier.starts_with("jsr:") {
            let store = match &self.store_root {
                Some(s) => s.clone(),
                None => {
                    return Err(JsErrorBox::generic(
                        "this runtime has no package store configured (INKA_STORE is unset); \
                         run `inka pkg seed` to install vendored packages"
                            .to_string(),
                    ))
                }
            };
            return match store_lookup(&store, specifier) {
                Ok(path) => file_url_response(&path),
                Err(e) => Err(JsErrorBox::generic(e)),
            };
        }
        // Hard offline: no remote module fetching, ever.
        if specifier.starts_with("http://") || specifier.starts_with("https://") {
            return Err(JsErrorBox::generic(format!(
                "network module imports are disabled ('{specifier}'); \
                 vendor the package with `inka pkg seed` instead"
            )));
        }
        // Bare imports are only valid from inside the store (a vendored
        // package importing one of its installed dependencies).
        if is_bare(specifier) {
            let store = match &self.store_root {
                Some(s) => s.clone(),
                None => {
                    return Err(JsErrorBox::generic(format!(
                        "bare import '{specifier}' is not supported; prefix it with npm: or jsr:"
                    )))
                }
            };
            let ref_path = referrer_file_path(referrer).unwrap_or_default();
            if !ref_path.starts_with(&store) {
                return Err(JsErrorBox::generic(format!(
                    "bare import '{specifier}' from a module outside the package store \
                     is not supported; prefix it with npm: or jsr:"
                )));
            }
            return match store_bare_lookup(&store, specifier, &ref_path) {
                Ok(path) => file_url_response(&path),
                Err(e) => Err(JsErrorBox::generic(e)),
            };
        }
        // Relative / file / node: / data: specifiers resolve as before.
        deno_core::resolve_import(specifier, referrer).map_err(JsErrorBox::from_err)
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<&deno_core::ModuleLoadReferrer>,
        options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        let specifier = module_specifier.clone();
        let artifact_root = self.artifact_root.clone();
        let store_root = self.store_root.clone();
        let fut = async move {
            let mut path = module_url_to_path(&specifier)?;
            let in_artifact = path.starts_with(&artifact_root);
            let in_store = store_root
                .as_ref()
                .is_some_and(|s| path.starts_with(s));
            if !in_artifact && !in_store {
                return Err(JsErrorBox::generic(format!(
                    "refusing to load module outside the artifact tree and package store: {specifier}"
                )));
            }
            // Deno-style resolution: an extensionless specifier like "./math"
            // may point at math.ts / math.js / ...
            if !path.is_file() && path.extension().is_none() {
                const EXTS: [&str; 6] = ["ts", "mts", "cts", "js", "mjs", "json"];
                let mut found = None;
                for ext in EXTS {
                    let cand = PathBuf::from(format!("{}.{ext}", path.to_string_lossy()));
                    if cand.is_file() {
                        found = Some(cand);
                        break;
                    }
                }
                if let Some(p) = found {
                    path = p;
                }
            }
            let in_artifact = path.starts_with(&artifact_root);
            let in_store = store_root
                .as_ref()
                .is_some_and(|s| path.starts_with(s));
            if !in_artifact && !in_store {
                return Err(JsErrorBox::generic(format!(
                    "refusing to load module outside the artifact tree and package store: {specifier}"
                )));
            }
            let bytes = std::fs::read(&path).map_err(|source| {
                JsErrorBox::from_err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("Cannot load module \"{specifier}\": {source}"),
                ))
            })?;

            let module_type = if let Some(extension) = path.extension() {
                let ext = extension.to_string_lossy().to_lowercase();
                if ext == "json" {
                    ModuleType::Json
                } else {
                    match &options.requested_module_type {
                        deno_core::RequestedModuleType::Other(ty) => {
                            ModuleType::Other(ty.clone())
                        }
                        deno_core::RequestedModuleType::Text => ModuleType::Text,
                        deno_core::RequestedModuleType::Bytes => ModuleType::Bytes,
                        _ => ModuleType::JavaScript,
                    }
                }
            } else {
                ModuleType::JavaScript
            };

            if options.requested_module_type == RequestedModuleType::Json
                && module_type != ModuleType::Json
            {
                return Err(JsErrorBox::type_error(format!(
                    "Expected a JSON module, but identified a {module_type} module.\n  Specifier: {specifier}"
                )));
            }
            if module_type == ModuleType::Json
                && options.requested_module_type != RequestedModuleType::Json
            {
                return Err(JsErrorBox::generic(
                    "Attempted to load JSON module without specifying \"type\": \"json\" attribute in the import statement.",
                ));
            }

            // Transpile TS-family files (decided by the resolved file's
            // extension, which also covers extensionless specifiers).
            let file_ts = path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .is_some_and(|e| e == "ts" || e == "mts" || e == "cts");
            let code: ModuleSourceCode = if module_type == ModuleType::JavaScript && file_ts {
                // TypeScript source: transpile to JS before handing it to V8.
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let file_url = ModuleSpecifier::from_file_path(&path)
                    .unwrap_or_else(|_| specifier.clone());
                let name = ModuleName::from(file_url.as_str().to_string());
                let source = ModuleCodeString::from(text);
                let (js, _map) = maybe_transpile_source(name, source).map_err(|e| {
                    JsErrorBox::generic(format!(
                        "failed to transpile TypeScript module {specifier}: {e}"
                    ))
                })?;
                ModuleSourceCode::String(js)
            } else {
                ModuleSourceCode::Bytes(bytes.into_boxed_slice().into())
            };

            Ok(ModuleSource::new(module_type, code, &specifier, None))
        };

        ModuleLoadResponse::Async(Box::pin(fut))
    }
}

fn module_url_to_path(specifier: &ModuleSpecifier) -> Result<PathBuf, JsErrorBox> {
    specifier.to_file_path().map_err(|_| {
        JsErrorBox::type_error(format!("not a file URL module: {specifier}"))
    })
}

// ---- npm/node trait slots --------------------------------------------------
// This build ships the full Deno.* / Web surface but no `node:`/`npm:` module
// resolution. The generic slots below are required by MainWorker's API and are
// never invoked when node_services is None.

#[derive(Clone, Debug)]
struct NoNpm;

impl InNpmPackageChecker for NoNpm {
    fn in_npm_package(&self, _specifier: &Url) -> bool {
        false
    }
}

#[derive(Clone, Debug)]
struct NoNpmFolder;

impl NpmPackageFolderResolver for NoNpmFolder {
    fn resolve_package_folder_from_package(
        &self,
        _specifier: &str,
        _referrer: &UrlOrPathRef,
    ) -> Result<PathBuf, errors::PackageFolderResolveError> {
        unreachable!("npm package resolution is not supported in this inka runtime build")
    }

    fn resolve_types_package_folder(
        &self,
        _types_package_name: &str,
        _maybe_package_version: Option<&deno_semver::Version>,
        _maybe_referrer: Option<&UrlOrPathRef>,
    ) -> Option<PathBuf> {
        None
    }
}

type DrtServices = WorkerServiceOptions<NoNpm, NoNpmFolder, RealSys>;

fn build_services(
    permissions: PermissionsContainer,
    loader: Rc<dyn ModuleLoader>,
) -> DrtServices {
    WorkerServiceOptions {
        blob_store: BlobStore::default_arc(),
        broadcast_channel: InMemoryBroadcastChannel::default(),
        deno_rt_native_addon_loader: None,
        feature_checker: Arc::new(FeatureChecker::default()),
        fs: Arc::new(RealFs) as Arc<dyn FileSystem>,
        module_loader: loader,
        node_services: None,
        npm_process_state_provider: None,
        permissions,
        root_cert_store_provider: None,
        fetch_dns_resolver: FetchDnsResolver::default(),
        shared_array_buffer_store: None,
        compiled_wasm_module_store: None,
        v8_code_cache: None,
        bundle_provider: None,
    }
}

// ---- permissions ------------------------------------------------------------
// Manifest permission lines are forwarded by the launcher as a newline-joined
// string: `permissions=all|none`, `allow-<cat>=<list>`, `deny-<cat>=<list>`.
//
// Policy (deny-by-default):
//   - empty DSL / permissions=none -> deny everything
//   - permissions=all             -> allow everything
//   - allow-<cat>                 -> grant that category; unmentioned denied
//   - deny-<cat>                  -> trims an allowed category (allow-* or
//                                    permissions=all); no-op + warning otherwise
// Lists are comma/whitespace separated; `*` means "all" in that category.

const PERM_CATEGORIES: [&str; 7] = ["read", "write", "net", "env", "run", "sys", "ffi"];

#[derive(Default)]
struct PermSpec {
    /// `permissions=all` — everything allowed (then trimmed by any deny-*).
    all: bool,
    allow: Vec<(String, Vec<String>)>,
    deny: Vec<(String, Vec<String>)>,
}

fn parse_perm_dsl(dsl: &str) -> Result<PermSpec, String> {
    let mut spec = PermSpec::default();
    for line in dsl.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("malformed permission line: '{line}'"));
        };
        let key = key.trim();
        let value = value.trim();
        if key == "permissions" {
            match value {
                "all" => spec.all = true,
                "none" | "" => {}
                other => {
                    return Err(format!(
                        "unknown permissions mode '{other}' (expected 'all' or 'none')"
                    ))
                }
            }
            continue;
        }
        let (kind, cat) = if let Some(cat) = key.strip_prefix("allow-") {
            ("allow", cat)
        } else if let Some(cat) = key.strip_prefix("deny-") {
            ("deny", cat)
        } else {
            continue; // non-permission keys are not part of the forwarded DSL
        };
        if !PERM_CATEGORIES.contains(&cat) {
            return Err(format!("unknown permission category in '{key}'"));
        }
        let items: Vec<String> = value
            .split([',', ' '])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        if kind == "allow" {
            spec.allow.push((cat.to_string(), items));
        } else {
            spec.deny.push((cat.to_string(), items));
        }
    }
    Ok(spec)
}

fn find<'a>(list: &'a [(String, Vec<String>)], cat: &str) -> Option<&'a Vec<String>> {
    list.iter()
        .find(|(c, _)| c == cat)
        .map(|(_, v)| v)
}

/// `*` in a manifest value means "all in this category", which at the options
/// layer is expressed as an empty vector (see `global_from_option`).
fn expand(items: &[String]) -> Vec<String> {
    if items.iter().any(|s| s == "*") {
        Vec::new()
    } else {
        items.to_vec()
    }
}

fn permissions_from_dsl(dsl: &str) -> Result<PermissionsContainer, String> {
    let parser: Arc<dyn PermissionDescriptorParser> =
        Arc::new(RuntimePermissionDescriptorParser::new(RealSys));
    let spec = parse_perm_dsl(dsl)?;
    let perms = if spec.all && spec.deny.is_empty() {
        Permissions::allow_all()
    } else {
        build_options_permissions(parser.as_ref(), &spec)?
    };
    Ok(PermissionsContainer::new(parser, perms))
}

fn build_options_permissions(
    parser: &dyn PermissionDescriptorParser,
    spec: &PermSpec,
) -> Result<Permissions, String> {
    // Deny-by-default: a category is allowed only if listed in allow-*, or
    // globally when `permissions=all` (which is then trimmed by deny-*).
    let cat_allow = |cat: &str| -> Option<Vec<String>> {
        match find(&spec.allow, cat) {
            Some(items) => Some(expand(items)),
            None if spec.all => Some(Vec::new()), // global allow under permissions=all
            None => None,                         // deny-by-default
        }
    };
    let cat_deny = |cat: &str| -> Option<Vec<String>> {
        find(&spec.deny, cat).map(|items| expand(items))
    };

    // A deny with no allow in that category cannot trim anything under
    // deny-by-default; surface it so the manifest author isn't misled.
    for (cat, _) in &spec.deny {
        if find(&spec.allow, cat).is_none() && !spec.all {
            eprintln!(
                "[inka] warning: deny-{cat} has no effect without allow-{cat} or permissions=all \
                 (deny-by-default is already in force)"
            );
        }
    }

    let opts = PermissionsOptions {
        prompt: false,
        allow_read: cat_allow("read"),
        deny_read: cat_deny("read"),
        allow_write: cat_allow("write"),
        deny_write: cat_deny("write"),
        allow_net: cat_allow("net"),
        deny_net: cat_deny("net"),
        allow_env: cat_allow("env"),
        deny_env: cat_deny("env"),
        allow_run: cat_allow("run"),
        deny_run: cat_deny("run"),
        allow_sys: cat_allow("sys"),
        deny_sys: cat_deny("sys"),
        allow_ffi: cat_allow("ffi"),
        deny_ffi: cat_deny("ffi"),
        ..Default::default()
    };

    Permissions::from_options(parser, &opts).map_err(|e| format!("invalid permissions: {e}"))
}

async fn run_module_async(
    main_module: &ModuleSpecifier,
    args: &[String],
    permissions: PermissionsContainer,
    loader: Rc<dyn ModuleLoader>,
) -> Result<i32, String> {
    let services = build_services(permissions, loader);
    let mut options = WorkerOptions::default();
    options.bootstrap.args = args.to_vec();
    options.bootstrap.location = Some(main_module.clone());
    options.bootstrap.log_level = WorkerLogLevel::Error;
    options.startup_snapshot = Some(STARTUP_SNAPSHOT);
    options.residual_lazy_js_sources = runtime_snapshot::RESIDUAL_LAZY_JS;
    options.residual_lazy_esm_sources = runtime_snapshot::RESIDUAL_LAZY_ESM;

    let mut worker = MainWorker::bootstrap_from_options(main_module, services, options);

    if let Err(e) = worker.execute_main_module(main_module).await {
        return Err(format!("{e}"));
    }
    if let Err(e) = worker.run_event_loop(false).await {
        return Err(format!("{e}"));
    }
    if let Err(e) = worker.dispatch_load_event() {
        eprintln!("[inka] load event error: {e}");
    }
    if let Err(e) = worker.run_event_loop(false).await {
        return Err(format!("{e}"));
    }
    let _ = worker.dispatch_beforeunload_event();
    if let Err(e) = worker.run_event_loop(false).await {
        return Err(format!("{e}"));
    }
    let _ = worker.dispatch_process_beforeexit_event();
    if let Err(e) = worker.run_event_loop(false).await {
        return Err(format!("{e}"));
    }
    let exit_code = worker.exit_code();
    let _ = worker.dispatch_unload_event();
    let _ = worker.dispatch_process_exit_event();
    Ok(exit_code)
}

fn ts_family(name: &str) -> bool {
    let ext = name.rsplit('.').next().map(|e| e.to_ascii_lowercase());
    matches!(ext.as_deref(), Some("ts" | "mts" | "cts"))
}

/// Transpile a single-file TypeScript entry to JavaScript before staging.
/// Plain JS (and anything whose name is not TS-family) passes through unchanged.
fn transpile_ts_source(
    module: &str,
    source: &[u8],
    specifier: &ModuleSpecifier,
) -> Result<Vec<u8>, String> {
    if !ts_family(module) {
        return Ok(source.to_vec());
    }
    let text = String::from_utf8_lossy(source).into_owned();
    let name = ModuleName::from(specifier.as_str().to_string());
    let code = ModuleCodeString::from(text);
    let (js, _map) = maybe_transpile_source(name, code)
        .map_err(|e| format!("failed to transpile TypeScript module '{module}': {e}"))?;
    Ok(js.as_bytes().to_vec())
}

/// Runs an entry module from a tree (`dir`/`entry`) through the store-aware
/// `PkgLoader`. Used by both multi-file artifacts and the staged single-entry
/// trees that `run_inner` builds.
fn run_tree(
    dir: &str,
    entry: &str,
    args: &[String],
    perm_dsl: Option<&str>,
) -> Result<i32, String> {
    let root = PathBuf::from(dir);
    if !root.is_dir() {
        return Err(format!("runtime directory not found: {dir}"));
    }
    if entry.is_empty() || entry.contains("..") || Path::new(entry).is_absolute() {
        return Err(format!("invalid entry path '{entry}'"));
    }
    let file = root.join(entry);
    if !file.is_file() {
        return Err(format!("entry module not found in artifact tree: {entry}"));
    }

    let permissions = permissions_from_dsl(perm_dsl.unwrap_or(""))?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to build tokio runtime: {e}"))?;

    rt.block_on(async {
        let url = ModuleSpecifier::from_file_path(&file)
            .map_err(|_| format!("failed to derive file url for {entry}"))?;
        let loader: Rc<dyn ModuleLoader> = Rc::new(PkgLoader {
            artifact_root: root,
            store_root: store_root_env(),
        });
        run_module_async(&url, args, permissions, loader).await
    })
}

fn run_inner(
    module: &str,
    source: &[u8],
    args: &[String],
    perm_dsl: Option<&str>,
) -> Result<i32, String> {
    let nonce = format!("{}-{}", std::process::id(), args.len());

    // Stage the single entry as its own one-file tree so it goes through the
    // same store-aware loader path as multi-file artifacts (so npm:/jsr:
    // imports work identically in both). TS entries are transpiled to JS first.
    let dir = std::env::temp_dir().join(format!("inka-{nonce}"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("failed to stage module tree: {e}"))?;

    let entry = "main.js";
    let bytes = if ts_family(module) {
        let fake_ts = dir.join("entry.ts");
        let spec = ModuleSpecifier::from_file_path(&fake_ts)
            .map_err(|_| "failed to derive specifier for TypeScript module".to_string())?;
        transpile_ts_source(module, source, &spec)?
    } else {
        source.to_vec()
    };
    if let Err(e) = std::fs::write(dir.join(entry), &bytes) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(format!("failed to stage module: {e}"));
    }

    let dir_str = dir.to_string_lossy().into_owned();
    let result = run_tree(&dir_str, entry, args, perm_dsl);
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// Runs an entry module from a staged multi-file artifact tree (`dir`/`entry`),
/// resolving relative imports and vendored packages via `PkgLoader`.
fn run_dir_inner(
    dir: &str,
    entry: &str,
    args: &[String],
    perm_dsl: Option<&str>,
) -> Result<i32, String> {
    run_tree(dir, entry, args, perm_dsl)
}

// ---- version ---------------------------------------------------------------

fn version_cstr() -> &'static CStr {
    static V: OnceLock<CString> = OnceLock::new();
    V.get_or_init(|| {
        CString::new(format!("inka_runtime-{DENO_RUNTIME_VERSION}"))
            .expect("nul in version string")
    })
}

#[no_mangle]
pub extern "C" fn inka_runtime_version() -> *const c_char {
    version_cstr().as_ptr()
}

// ---- handle ----------------------------------------------------------------

#[no_mangle]
pub extern "C" fn inka_runtime_create() -> *mut c_void {
    Box::into_raw(Box::new(())) as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn inka_runtime_destroy(rt: *mut c_void) {
    if !rt.is_null() {
        drop(Box::from_raw(rt as *mut ()));
    }
}

// ---- run -------------------------------------------------------------------

unsafe fn set_err_msg(out: *mut *mut c_char, msg: String) {
    if out.is_null() {
        return;
    }
    let c = CString::new(msg).unwrap_or_else(|_| CString::new("error").unwrap());
    *out = Box::into_raw(c.into_boxed_c_str()) as *mut c_char;
}

unsafe fn run_from_raw(
    specifier: *const c_char,
    source: *const c_char,
    source_len: usize,
    argc: c_int,
    argv: *const *const c_char,
    exit_code: *mut c_int,
    err_msg: *mut *mut c_char,
    perms: Option<*const c_char>,
) -> c_int {
    if exit_code.is_null() {
        return -1;
    }
    *exit_code = 0;
    if !err_msg.is_null() {
        *err_msg = std::ptr::null_mut();
    }

    let specifier = if specifier.is_null() {
        String::new()
    } else {
        CStr::from_ptr(specifier).to_string_lossy().into_owned()
    };

    let src = if source.is_null() {
        &[][..]
    } else {
        std::slice::from_raw_parts(source as *const u8, source_len)
    };

    let mut args = Vec::new();
    if !argv.is_null() {
        for i in 0..argc {
            let p = *argv.add(i as usize);
            if p.is_null() {
                break;
            }
            args.push(CStr::from_ptr(p).to_string_lossy().into_owned());
        }
    }

    let dsl = match perms {
        Some(p) if !p.is_null() => Some(CStr::from_ptr(p).to_string_lossy().into_owned()),
        _ => None,
    };

    match run_inner(&specifier, src, &args, dsl.as_deref()) {
        Ok(code) => {
            *exit_code = code;
            0
        }
        Err(e) => {
            *exit_code = 1;
            set_err_msg(err_msg, e);
            1
        }
    }
}

/// Legacy run entry point (same signature as the original ABI). Behaves like
/// `inka_runtime_run_module_perm` with empty permissions: deny-by-default.
#[no_mangle]
pub unsafe extern "C" fn inka_runtime_run_module(
    _rt: *mut c_void,
    specifier: *const c_char,
    source: *const c_char,
    source_len: usize,
    argc: c_int,
    argv: *const *const c_char,
    exit_code: *mut c_int,
    err_msg: *mut *mut c_char,
) -> c_int {
    run_from_raw(
        specifier, source, source_len, argc, argv, exit_code, err_msg, None,
    )
}

/// Permission-aware run entry point. `perms` is a newline-joined string of
/// manifest permission lines (or null/empty for deny-by-default).
#[no_mangle]
pub unsafe extern "C" fn inka_runtime_run_module_perm(
    _rt: *mut c_void,
    specifier: *const c_char,
    source: *const c_char,
    source_len: usize,
    argc: c_int,
    argv: *const *const c_char,
    exit_code: *mut c_int,
    err_msg: *mut *mut c_char,
    perms: *const c_char,
) -> c_int {
    run_from_raw(
        specifier,
        source,
        source_len,
        argc,
        argv,
        exit_code,
        err_msg,
        Some(perms),
    )
}

/// Multi-file run entry point: executes `entry` (a path relative to the
/// extracted `dir_path`) from a staged artifact tree, resolving its relative
/// imports. `perms` behaves like `inka_runtime_run_module_perm`.
#[no_mangle]
pub unsafe extern "C" fn inka_runtime_run_module_dir(
    _rt: *mut c_void,
    dir_path: *const c_char,
    entry: *const c_char,
    argc: c_int,
    argv: *const *const c_char,
    exit_code: *mut c_int,
    err_msg: *mut *mut c_char,
    perms: *const c_char,
) -> c_int {
    if exit_code.is_null() {
        return -1;
    }
    *exit_code = 0;
    if !err_msg.is_null() {
        *err_msg = std::ptr::null_mut();
    }

    let dir = if dir_path.is_null() {
        String::new()
    } else {
        CStr::from_ptr(dir_path).to_string_lossy().into_owned()
    };
    let entry = if entry.is_null() {
        String::new()
    } else {
        CStr::from_ptr(entry).to_string_lossy().into_owned()
    };

    let mut args = Vec::new();
    if !argv.is_null() {
        for i in 0..argc {
            let p = *argv.add(i as usize);
            if p.is_null() {
                break;
            }
            args.push(CStr::from_ptr(p).to_string_lossy().into_owned());
        }
    }
    let dsl = if perms.is_null() {
        None
    } else {
        Some(CStr::from_ptr(perms).to_string_lossy().into_owned())
    };

    match run_dir_inner(&dir, &entry, &args, dsl.as_deref()) {
        Ok(code) => {
            *exit_code = code;
            0
        }
        Err(e) => {
            *exit_code = 1;
            set_err_msg(err_msg, e);
            1
        }
    }
}

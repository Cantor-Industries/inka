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
use sys_traits::impls::RealSys;

const DENO_RUNTIME_VERSION: &str = "0.266.0";

static STARTUP_SNAPSHOT: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/CLI_SNAPSHOT.bin"));

mod runtime_snapshot {
    include!(concat!(env!("OUT_DIR"), "/EXTENSION_RESIDUAL_SOURCES.rs"));
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
    /// Root of the global package store (`<store>/node_modules/<name>/…`, one
    /// hoisted pool).
    store_root: Option<PathBuf>,
    /// True when the artifact's `.ts/.mts/.cts` payloads were transpiled at
    /// build time (`--transpile`); such files are served as plain JS.
    precompiled: bool,
}

fn store_root_env() -> Option<PathBuf> {
    std::env::var_os("INKA_STORE").map(PathBuf::from)
}

fn precompiled_flag() -> bool {
    std::env::var_os("INKA_PRECOMPILED").is_some()
}

fn has_scheme(spec: &str) -> bool {
    spec.split_once(':').is_some()
}

fn resolver_path_env() -> Option<PathBuf> {
    std::env::var_os("INKA_RESOLVER").map(PathBuf::from)
}

type FnResolve = unsafe extern "C" fn(
    *const c_char,
    *const c_char,
    *const c_char,
    *mut *mut c_char,
    *mut *mut c_char,
) -> c_int;
type FnFree = unsafe extern "C" fn(*mut c_char);
type FnAbi = unsafe extern "C" fn() -> c_int;

const RESOLVER_ABI: c_int = 1;
const KIND_USE_DEFAULT: c_int = 0;
const KIND_FILE: c_int = 1;
const KIND_BUILTIN: c_int = 2;
const KIND_ERROR: c_int = 3;

struct ResolverApi {
    resolve: FnResolve,
    free: FnFree,
}

/// Loaded once per process from `$INKA_RESOLVER` (set by the launcher). The
/// library is leaked after copying the function pointers, so only the
/// fn-pointer values live on.
static RESOLVER: OnceLock<Result<&'static ResolverApi, &'static str>> = OnceLock::new();

fn resolver_api() -> Result<&'static ResolverApi, &'static str> {
    RESOLVER.get_or_init(init_resolver).clone()
}

fn init_resolver() -> Result<&'static ResolverApi, &'static str> {
    let path = resolver_path_env().ok_or(
        "no inka resolver configured (INKA_RESOLVER is unset); install it with `inka install`",
    )?;
    // Safety: we dlopen a path supplied by the launcher (or INKA_RESOLVER) and
    // read the exported resolver symbols below.
    let lib = unsafe { libloading::Library::new(&path) }
        .map_err(|_| "failed to load the inka resolver library (libinka_resolver)")?;
    unsafe {
        let abi: libloading::Symbol<FnAbi> = lib
            .get(b"inka_resolver_abi")
            .map_err(|_| "missing inka_resolver_abi in resolver library")?;
        if abi() != RESOLVER_ABI {
            return Err("inka resolver ABI mismatch (expected 1); run `inka install` to update");
        }
    }
    let resolve: FnResolve = unsafe {
        let s: libloading::Symbol<FnResolve> = lib
            .get(b"inka_resolver_resolve")
            .map_err(|_| "missing inka_resolver_resolve in resolver library")?;
        *s
    };
    let free: FnFree = unsafe {
        let s: libloading::Symbol<FnFree> = lib
            .get(b"inka_resolver_free")
            .map_err(|_| "missing inka_resolver_free in resolver library")?;
        *s
    };
    // Keep the underlying library alive for the process lifetime.
    std::mem::forget(lib);
    Ok(Box::leak(Box::new(ResolverApi { resolve, free })))
}

fn cstring(s: &str) -> CString {
    CString::new(s).unwrap_or_else(|_| CString::new("").expect("empty has no nul"))
}

/// Ask the resolver library how to resolve `specifier`, then translate its
/// decision into a `ModuleResolveResponse`.
fn resolve_with_resolver(
    api: &ResolverApi,
    store: Option<&Path>,
    specifier: &str,
    referrer: &str,
) -> ModuleResolveResponse {
    let store_s = store
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let store_c = cstring(&store_s);
    let referrer_c = cstring(referrer);
    let spec_c = cstring(specifier);
    let mut a: *mut c_char = std::ptr::null_mut();
    let mut b: *mut c_char = std::ptr::null_mut();
    // Safety: fn pointers come from a compatible, process-lifetime library.
    let kind = unsafe {
        (api.resolve)(
            store_c.as_ptr(),
            referrer_c.as_ptr(),
            spec_c.as_ptr(),
            &mut a,
            &mut b,
        )
    };
    let a_str = if a.is_null() {
        String::new()
    } else {
        // Safety: `a` is owned by the resolver; read before freeing it below.
        unsafe { CStr::from_ptr(a).to_string_lossy().into_owned() }
    };
    unsafe {
        if !a.is_null() {
            (api.free)(a);
        }
        if !b.is_null() {
            (api.free)(b);
        }
    }
    match kind {
        KIND_USE_DEFAULT => deno_core::resolve_import(specifier, referrer).map_err(JsErrorBox::from_err),
        KIND_FILE => file_url_response(&PathBuf::from(a_str)),
        KIND_BUILTIN => {
            if a_str.is_empty() {
                Err(JsErrorBox::generic(
                    "resolver returned an empty built-in specifier",
                ))
            } else {
                deno_core::resolve_import(&a_str, referrer).map_err(JsErrorBox::from_err)
            }
        }
        KIND_ERROR => Err(JsErrorBox::generic(if a_str.is_empty() {
            format!("cannot resolve module '{specifier}'")
        } else {
            a_str
        })),
        other => Err(JsErrorBox::generic(format!(
            "resolver returned an unknown decision ({other}) for '{specifier}'"
        ))),
    }
}

/// Minimal resolution used when the resolver library is not installed: only
/// relative/file/node:/data: imports and the offline http(s) rejection remain;
/// store/bare resolution reports why the resolver is needed.
fn fallback_resolve(specifier: &str, referrer: &str, reason: &str) -> ModuleResolveResponse {
    if specifier.starts_with("npm:") || specifier.starts_with("jsr:") {
        return Err(JsErrorBox::generic(format!(
            "vendored package resolution requires the inka resolver ({reason})"
        )));
    }
    if specifier.starts_with("http://") || specifier.starts_with("https://") {
        return Err(JsErrorBox::generic(format!(
            "network module imports are disabled ('{specifier}'); \
             vendor the package with `inka pkg seed` instead"
        )));
    }
    if has_scheme(specifier)
        || specifier.starts_with("./")
        || specifier.starts_with("../")
        || specifier.starts_with('/')
    {
        return deno_core::resolve_import(specifier, referrer).map_err(JsErrorBox::from_err);
    }
    Err(JsErrorBox::generic(format!(
        "bare import '{specifier}' cannot be resolved ({reason})"
    )))
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
        // All import-resolution policy lives in the standalone inka resolver
        // (libinka_resolver). When it isn't installed we degrade to the small
        // built-in fallback (relative/file/node:/data: plus offline rejection).
        match resolver_api() {
            Ok(api) => resolve_with_resolver(api, self.store_root.as_deref(), specifier, referrer),
            Err(reason) => fallback_resolve(specifier, referrer, reason),
        }
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
        let precompiled = self.precompiled;
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
            // extension, which also covers extensionless specifiers). A
            // precompiled archive already carries JS under .ts/.mts/.cts
            // payload names, so those are served as-is (no runtime transpile).
            let file_ts = path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .is_some_and(|e| e == "ts" || e == "mts" || e == "cts");
            let code: ModuleSourceCode = if module_type == ModuleType::JavaScript && file_ts {
                if precompiled {
                    if std::env::var_os("INKA_DEBUG").is_some() {
                        eprintln!("[inka] precompiled module (no transpile): {specifier}");
                    }
                    ModuleSourceCode::Bytes(bytes.into_boxed_slice().into())
                } else {
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
                }
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
            precompiled: precompiled_flag(),
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

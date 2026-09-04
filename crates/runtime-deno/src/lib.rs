// dex runtime-deno: a cdylib embedding deno_runtime behind the frozen dex C ABI.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::OnceLock;

use deno_runtime::deno_core::url::Url;
use deno_runtime::deno_core::{FsModuleLoader, ModuleCodeString, ModuleLoader, ModuleName,
    ModuleSpecifier};
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
use sys_traits::impls::RealSys;

const DENO_RUNTIME_VERSION: &str = "0.266.0";

static STARTUP_SNAPSHOT: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/CLI_SNAPSHOT.bin"));

mod runtime_snapshot {
    include!(concat!(env!("OUT_DIR"), "/EXTENSION_RESIDUAL_SOURCES.rs"));
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
        unreachable!("npm package resolution is not supported in this dex runtime build")
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

fn build_services(permissions: PermissionsContainer) -> DrtServices {
    WorkerServiceOptions {
        blob_store: BlobStore::default_arc(),
        broadcast_channel: InMemoryBroadcastChannel::default(),
        deno_rt_native_addon_loader: None,
        feature_checker: Arc::new(FeatureChecker::default()),
        fs: Arc::new(RealFs) as Arc<dyn FileSystem>,
        module_loader: Rc::new(FsModuleLoader) as Rc<dyn ModuleLoader>,
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
                "[dex] warning: deny-{cat} has no effect without allow-{cat} or permissions=all \
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
) -> Result<i32, String> {
    let services = build_services(permissions);
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
        eprintln!("[dex] load event error: {e}");
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

fn run_inner(
    module: &str,
    source: &[u8],
    args: &[String],
    perm_dsl: Option<&str>,
) -> Result<i32, String> {
    let permissions = permissions_from_dsl(perm_dsl.unwrap_or(""))?;
    let nonce = format!("{}-{}", std::process::id(), args.len());

    // For TypeScript entries we need a real file-URL specifier so the runtime
    // transpiler can classify the media type and name the module; the actual
    // file we execute is always the transpiled JavaScript below.
    let js = if ts_family(module) {
        let fake_ts = std::env::temp_dir().join(format!("dex-{nonce}.ts"));
        let spec = ModuleSpecifier::from_file_path(&fake_ts)
            .map_err(|_| "failed to derive specifier for TypeScript module".to_string())?;
        transpile_ts_source(module, source, &spec)?
    } else {
        source.to_vec()
    };

    let path = std::env::temp_dir().join(format!("dex-{nonce}.js"));
    std::fs::write(&path, &js).map_err(|e| format!("failed to stage module: {e}"))?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to build tokio runtime: {e}"))?;

    let result = rt.block_on(async {
        let url = ModuleSpecifier::from_file_path(&path)
            .map_err(|_| "failed to derive file url for staged module".to_string())?;
        run_module_async(&url, args, permissions).await
    });

    let _ = std::fs::remove_file(&path);
    result
}

// ---- version ---------------------------------------------------------------

fn version_cstr() -> &'static CStr {
    static V: OnceLock<CString> = OnceLock::new();
    V.get_or_init(|| {
        CString::new(format!("deno_runtime-{DENO_RUNTIME_VERSION}"))
            .expect("nul in version string")
    })
}

#[no_mangle]
pub extern "C" fn dex_runtime_version() -> *const c_char {
    version_cstr().as_ptr()
}

// ---- handle ----------------------------------------------------------------

#[no_mangle]
pub extern "C" fn dex_runtime_create() -> *mut c_void {
    Box::into_raw(Box::new(())) as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn dex_runtime_destroy(rt: *mut c_void) {
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
/// `dex_runtime_run_module_perm` with empty permissions: deny-by-default.
#[no_mangle]
pub unsafe extern "C" fn dex_runtime_run_module(
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
/// manifest permission lines (or null/empty for allow-all).
#[no_mangle]
pub unsafe extern "C" fn dex_runtime_run_module_perm(
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

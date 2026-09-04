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
    PermissionDescriptorParser, PermissionsContainer, RuntimePermissionDescriptorParser,
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

fn build_services() -> DrtServices {
    let parser: Arc<dyn PermissionDescriptorParser> =
        Arc::new(RuntimePermissionDescriptorParser::new(RealSys));
    WorkerServiceOptions {
        blob_store: BlobStore::default_arc(),
        broadcast_channel: InMemoryBroadcastChannel::default(),
        deno_rt_native_addon_loader: None,
        feature_checker: Arc::new(FeatureChecker::default()),
        fs: Arc::new(RealFs) as Arc<dyn FileSystem>,
        module_loader: Rc::new(FsModuleLoader) as Rc<dyn ModuleLoader>,
        node_services: None,
        npm_process_state_provider: None,
        permissions: PermissionsContainer::allow_all(parser),
        root_cert_store_provider: None,
        fetch_dns_resolver: FetchDnsResolver::default(),
        shared_array_buffer_store: None,
        compiled_wasm_module_store: None,
        v8_code_cache: None,
        bundle_provider: None,
    }
}

async fn run_module_async(
    main_module: &ModuleSpecifier,
    args: &[String],
) -> Result<i32, String> {
    let services = build_services();
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

fn run_inner(module: &str, source: &[u8], args: &[String]) -> Result<i32, String> {
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
        run_module_async(&url, args).await
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

    match run_inner(&specifier, src, &args) {
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

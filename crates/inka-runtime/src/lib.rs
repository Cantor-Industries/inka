// inka runtime: a cdylib embedding the Deno runtime (deno_runtime crate)
// behind the frozen inka C ABI.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::OnceLock;

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
use deno_runtime::transpile::maybe_transpile_source;
use deno_runtime::worker::{MainWorker, WorkerOptions, WorkerServiceOptions};
use deno_runtime::{FeatureChecker, WorkerLogLevel};

use deno_config::workspace::{
    WorkspaceDirectory, WorkspaceDiscoverOptions, WorkspaceDiscoverStart,
};
use deno_error::JsErrorBox;
use deno_resolver::workspace::{
    CreateResolverOptions, MappedResolution, ResolutionKind as WorkspaceResolutionKind,
    WorkspaceResolver,
};
use deno_terminal::colors;
use sys_traits::impls::RealSys;

/// Runtime tuple version, set by `build.rs` from `runtime-version`: the
/// `deno_runtime` base (`0.xxx.0`) plus an inka runtime revision (`.1`, `.2`, …).
const RUNTIME_VERSION: &str = env!("INKA_RUNTIME_VERSION");

static STARTUP_SNAPSHOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/CLI_SNAPSHOT.bin"));

mod runtime_snapshot {
    include!(concat!(env!("OUT_DIR"), "/EXTENSION_RESIDUAL_SOURCES.rs"));
}

mod node_services;
mod resolver;
mod tsconfig;

// deno_node registers ops that borrow `RealSys` from the isolate's op state
// (e.g. ops/process.rs, ops/require.rs), but deno_runtime only inserts that
// resource when node services are enabled. We always run with node_services
// None, so inject RealSys ourselves via a tiny state extension.
deno_core::extension!(
    inka_rt_state,
    state = |state: &mut deno_core::OpState| {
        state.put(sys_traits::impls::RealSys);
    }
);

/// Module loader rooted at the execution tree. Serves the artifact tree (or
/// the staged single-entry tree) as local files, plus `node:`/`data:`/`file:`
/// built-ins, and rejects network imports outright. Reading is confined to the
/// execution tree's real path; nothing outside it is ever served.
struct PkgLoader {
    /// Root of the extracted artifact tree.
    artifact_root: PathBuf,
    /// Permissions used to honor an explicit read grant for out-of-tree module
    /// reads (e.g. `npm link`); deny-by-default still confines otherwise.
    permissions: PermissionsContainer,
    /// CJS/Node services used to serve an ESM facade for CommonJS modules.
    node_services: node_services::NodeServices,
    /// Offline module-graph resolution (`inka run`): the cached `jsr:`/remote
    /// module graph, fed by the workspace import map.
    resolver: Rc<resolver::GraphResolverState>,
    /// Deno's workspace resolver: the authoritative import map +
    /// package.json/workspace mapping (the same policy Deno applies).
    workspace: Rc<deno_resolver::workspace::WorkspaceResolver<RealSys>>,
    /// Run-time `tsconfig` `baseUrl`/`paths` fallback (the same `oxc_resolver`
    /// `inka build` uses), applied to bare specifiers the import map leaves
    /// unmapped.
    tsconfig: tsconfig::Resolver,
}

impl ModuleLoader for PkgLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: deno_core::ResolutionKind,
    ) -> ModuleResolveResponse {
        let bare = !specifier.contains(':')
            && !specifier.starts_with("./")
            && !specifier.starts_with("../")
            && !specifier.starts_with('/');
        // 1. Deno's workspace resolver maps a bare specifier through the
        //    `deno.json` import map, package.json `#imports`, and workspace
        //    members — the same policy Deno applies.
        let mapped = if bare {
            url::Url::parse(referrer).ok().and_then(|referrer_url| {
                match self.workspace.resolve(
                    specifier,
                    &referrer_url,
                    WorkspaceResolutionKind::Execution,
                ) {
                    Ok(MappedResolution::Normal { specifier, .. })
                    | Ok(MappedResolution::WorkspaceJsrPackage { specifier, .. }) => {
                        Some(specifier)
                    }
                    _ => None,
                }
            })
        } else {
            None
        };

        // 2. A bare specifier the import map leaves unmapped may be a
        //    `tsconfig` `baseUrl`/`paths` alias (e.g. `src/util` that resolves
        //    against `"baseUrl": "."`). Resolve it with the same `oxc_resolver`
        //    `inka build` uses, confined to the execution tree. `#imports` is
        //    left to Deno's node resolver.
        if bare && mapped.is_none() && !specifier.starts_with('#') {
            if let Ok(referrer_url) = url::Url::parse(referrer) {
                if let Some(u) = self.tsconfig.resolve(specifier, &referrer_url) {
                    return Ok(u);
                }
            }
        }

        let spec = mapped.as_ref().map(|u| u.as_str()).unwrap_or(specifier);

        if spec.starts_with("npm:") {
            return self
                .node_services
                .resolve_specifier(spec, referrer)
                .map_err(JsErrorBox::generic);
        }
        if spec.starts_with("jsr:") {
            return self
                .resolver
                .resolve_in_graph(spec, referrer)
                .ok_or_else(|| {
                    JsErrorBox::generic(format!(
                        "jsr module not resolved from the Deno cache: {spec}"
                    ))
                });
        }
        if bare || mapped.is_some() {
            if let Some(u) = self.resolver.resolve_in_graph(spec, referrer) {
                return Ok(u);
            }
            return self
                .node_services
                .resolve_specifier(spec, referrer)
                .map_err(JsErrorBox::generic);
        }
        // Schemes, relative, and absolute specifiers. Network (`https:`) is
        // allowed as a specifier but served only from the cache by `load`.
        if spec.starts_with("http://") || spec.starts_with("https://") {
            return url::Url::parse(spec).map_err(|e| JsErrorBox::generic(e.to_string()));
        }
        self.node_services
            .resolve_specifier(spec, referrer)
            .map_err(JsErrorBox::generic)
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<&deno_core::ModuleLoadReferrer>,
        options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        let specifier = module_specifier.clone();
        let artifact_root = self.artifact_root.clone();
        let permissions = self.permissions.clone();
        let node_services = self.node_services.clone();
        let deno_dir = self.resolver.deno_dir().to_path_buf();
        let fut = async move {
            // Remote modules (jsr) are served from the Deno cache only.
            if specifier.scheme() == "https" || specifier.scheme() == "http" {
                return load_cached_remote(&specifier, &deno_dir).await;
            }
            let mut path = module_url_to_path(&specifier)?;
            // Deno-style resolution: an extensionless specifier like "./math"
            // may point at math.ts / math.js / ...
            if !path.is_file() && path.extension().is_none() {
                const EXTS: [&str; 8] = ["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "json"];
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
            // TS "write .js, ship .ts": a package `exports`/`main` (or any
            // `.js`-suffixed specifier) may resolve to a `.ts` source file that
            // doesn't have a `.js` sibling. Try the TypeScript equivalents.
            if !path.is_file() {
                for cand in ts_rewrite_candidates(&path) {
                    if cand.is_file() {
                        path = cand;
                        break;
                    }
                }
            }
            // Realpath confinement: the lexical path can be defeated by a
            // symlink planted inside the tree, so resolve symlinks and require
            // the real file to stay under the canonical execution root — unless
            // an explicit read grant covers it (out-of-tree `npm link`).
            let real = std::fs::canonicalize(&path).map_err(|source| {
                JsErrorBox::from_err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("Cannot load module \"{specifier}\": {source}"),
                ))
            })?;
            if !real.starts_with(&artifact_root)
                && !node_services::read_granted(&permissions, &real)
            {
                return Err(JsErrorBox::generic(format!(
                    "refusing to load module outside the execution tree: {specifier}"
                )));
            }
            path = real;
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
                        deno_core::RequestedModuleType::Other(ty) => ModuleType::Other(ty.clone()),
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

            // ESM import of a CommonJS module: serve a facade (default plus
            // statically-detected named exports) generated by Deno's CJS->ESM
            // translator. `require()` reads the original file through the CJS
            // loader, so this does not recurse back into the ESM loader.
            if module_type == ModuleType::JavaScript && node_services.maybe_cjs(&path) {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                if let Some(facade) =
                    node_services
                        .cjs_facade(&specifier, text)
                        .await
                        .map_err(|e| {
                            JsErrorBox::generic(format!(
                                "failed to convert CommonJS module {specifier} to ESM: {e}"
                            ))
                        })?
                {
                    return Ok(ModuleSource::new(
                        ModuleType::JavaScript,
                        ModuleSourceCode::String(ModuleCodeString::from(facade)),
                        &specifier,
                        None,
                    ));
                }
                // Otherwise it is an ES module; fall through to the normal path.
            }

            // Transpile TS-family files (decided by the resolved file's
            // extension, which also covers extensionless specifiers).
            let file_ts = ts_family(&path.to_string_lossy());
            let code: ModuleSourceCode = if module_type == ModuleType::JavaScript && file_ts {
                // TypeScript source: transpile to JS before handing it to V8.
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let file_url =
                    ModuleSpecifier::from_file_path(&path).unwrap_or_else(|_| specifier.clone());
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
    specifier
        .to_file_path()
        .map_err(|_| JsErrorBox::type_error(format!("not a file URL module: {specifier}")))
}

/// TypeScript's "write `.js`, ship `.ts`" convention: a `.js`/`.jsx`/`.mjs`/
/// `.cjs` specifier resolves to its TypeScript source sibling when the script
/// file itself does not exist. Mirrors rolldown's default `extension_alias`
/// table so `inka run` and `inka build` agree.
pub(crate) fn ts_rewrite_candidates(path: &Path) -> Vec<PathBuf> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let rewrites: &[&str] = match ext.as_deref() {
        Some("js") => &["ts", "tsx"],
        Some("jsx") => &["tsx", "ts"],
        Some("mjs") => &["mts"],
        Some("cjs") => &["cts"],
        _ => &[],
    };
    rewrites.iter().map(|r| path.with_extension(r)).collect()
}

/// Serve an `https:`/`http:` module from the Deno remote cache (offline). TS
/// modules (jsr) are transpiled on load.
async fn load_cached_remote(
    specifier: &ModuleSpecifier,
    deno_dir: &Path,
) -> Result<ModuleSource, JsErrorBox> {
    use deno_cache_dir::{GlobalHttpCache, HttpCache};

    let cache = GlobalHttpCache::new(RealSys, deno_dir.join("remote"));
    let key = cache
        .cache_item_key(specifier)
        .map_err(JsErrorBox::from_err)?;
    let entry = cache
        .get(&key, None)
        .map_err(JsErrorBox::from_err)?
        .ok_or_else(|| {
            JsErrorBox::generic(format!(
                "module not in the Deno cache (offline): {specifier}"
            ))
        })?;
    let bytes = entry.content.into_owned();

    let is_ts = ts_family(specifier.path());
    let code: ModuleSourceCode = if is_ts {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let name = ModuleName::from(specifier.as_str().to_string());
        let (js, _map) =
            maybe_transpile_source(name, ModuleCodeString::from(text)).map_err(|e| {
                JsErrorBox::generic(format!(
                    "failed to transpile TypeScript module {specifier}: {e}"
                ))
            })?;
        ModuleSourceCode::String(js)
    } else {
        ModuleSourceCode::Bytes(bytes.into_boxed_slice().into())
    };
    Ok(ModuleSource::new(
        ModuleType::JavaScript,
        code,
        specifier,
        None,
    ))
}

// ---- npm/node services -----------------------------------------------------
// The node services live in `node_services` (the single seam over
// Deno's `deno_node`/`node_resolver` API). `WorkerServiceOptions` still needs
// the checker/resolver generic parameters even though the concrete values are
// built inside that module.

type DrtServices = WorkerServiceOptions<
    node_services::ExecutionNpmChecker,
    node_services::ExecutionFolderResolver,
    RealSys,
>;

fn build_services(
    permissions: PermissionsContainer,
    loader: Rc<dyn ModuleLoader>,
    node_services: node_services::InkaNodeServices,
) -> DrtServices {
    WorkerServiceOptions {
        blob_store: BlobStore::default_arc(),
        broadcast_channel: InMemoryBroadcastChannel::default(),
        deno_rt_native_addon_loader: None,
        feature_checker: Arc::new(FeatureChecker::default()),
        fs: Arc::new(RealFs) as Arc<dyn FileSystem>,
        module_loader: loader,
        node_services: Some(node_services),
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

const PERM_CATEGORIES: [&str; 8] = ["read", "write", "net", "env", "run", "sys", "ffi", "import"];

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
        // Lists are comma-separated (Deno's separator). We deliberately do not
        // split on spaces, so descriptors that contain spaces survive intact.
        let items: Vec<String> = value
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        if kind == "allow" {
            // An empty allow list would otherwise widen to "allow all" at the
            // options layer. Reject it: use `*` (or `permissions=all`) to mean
            // all. This is defence in depth beyond the CLI/config checks.
            if items.is_empty() {
                return Err(format!(
                    "empty allow list in '{key}' (use '*' or 'permissions=all' to allow all)"
                ));
            }
            spec.allow.push((cat.to_string(), items));
        } else {
            spec.deny.push((cat.to_string(), items));
        }
    }
    Ok(spec)
}

fn find<'a>(list: &'a [(String, Vec<String>)], cat: &str) -> Option<&'a Vec<String>> {
    list.iter().find(|(c, _)| c == cat).map(|(_, v)| v)
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
    let cat_deny =
        |cat: &str| -> Option<Vec<String>> { find(&spec.deny, cat).map(|items| expand(items)) };

    // A deny with no allow in that category cannot trim anything under
    // deny-by-default; surface it so the manifest author isn't misled.
    for (cat, _) in &spec.deny {
        if find(&spec.allow, cat).is_none() && !spec.all {
            eprintln!(
                "{}: deny-{cat} has no effect without allow-{cat} or permissions=all \
                 (deny-by-default is already in force)",
                colors::yellow_bold("warning")
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
        // `import` grants Deno's import permission for cached remote/jsr modules;
        // network fetch stays disabled (see `resolve_specifier`).
        allow_import: cat_allow("import"),
        deny_import: cat_deny("import"),
        ..Default::default()
    };

    Permissions::from_options(parser, &opts).map_err(|e| format!("invalid permissions: {e}"))
}

async fn run_module_async(
    main_module: &ModuleSpecifier,
    args: &[String],
    permissions: PermissionsContainer,
    loader: Rc<dyn ModuleLoader>,
    node_services: node_services::InkaNodeServices,
) -> Result<i32, String> {
    let services = build_services(permissions, loader, node_services);
    let mut options = WorkerOptions::default();
    options.bootstrap.args = args.to_vec();
    // inka is bring-your-own-`node_modules` (BYONM): npm packages resolve from
    // the execution tree's `node_modules`, never Deno's global npm cache. This
    // flag drives `usesLocalNodeModulesDir` in deno_node's `require()`
    // resolution; without it CJS falls back to the deno-dir lookup path, which
    // mishandles out-of-tree (symlinked) packages. It does not affect the
    // remote (`jsr:`/`https:`) cache under `$DENO_DIR/remote`.
    options.bootstrap.has_node_modules_dir = true;
    // Leave bootstrap.location unset (like `deno run`): setting it makes the worker
    // expose a live `globalThis.location` whose origin is "null" for the staged
    // file:// module, which breaks web code that builds URL bases from it (e.g.
    // @effect/platform UrlParams.baseUrl() -> "null" + pathname -> invalid URL).
    options.bootstrap.log_level = WorkerLogLevel::Error;
    options.startup_snapshot = Some(STARTUP_SNAPSHOT);
    options.residual_lazy_js_sources = runtime_snapshot::RESIDUAL_LAZY_JS;
    options.residual_lazy_esm_sources = runtime_snapshot::RESIDUAL_LAZY_ESM;
    options.extensions = vec![inka_rt_state::init()];

    let mut worker = MainWorker::bootstrap_from_options(main_module, services, options);

    if let Err(e) = worker.execute_main_module(main_module).await {
        return Err(format!("{e}"));
    }
    if let Err(e) = worker.run_event_loop(false).await {
        return Err(format!("{e}"));
    }
    if let Err(e) = worker.dispatch_load_event() {
        eprintln!("{}: load event: {e}", colors::yellow_bold("warning"));
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

/// Configure terminal color once per process: `deno_terminal`'s `NO_COLOR`/
/// `FORCE_COLOR` policy, additionally gated on a TTY so piped output is plain.
fn init_terminal() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let forced = colors::force_color();
        let color = colors::use_color() && (forced || deno_terminal::is_stderr_tty());
        colors::set_use_color(color);
    });
}

/// Runs an entry module from a tree (`dir`/`entry`) through the `PkgLoader`.
fn run_tree(
    dir: &str,
    entry: &str,
    args: &[String],
    perm_dsl: Option<&str>,
) -> Result<i32, String> {
    init_terminal();
    let root = PathBuf::from(dir);
    if !root.is_dir() {
        return Err(format!("runtime directory not found: {dir}"));
    }
    if entry.is_empty() || entry.contains("..") || Path::new(entry).is_absolute() {
        return Err(format!("invalid entry path '{entry}'"));
    }
    // Canonicalize once: the module loader and node services confine every read
    // to this real path (symlinks inside the tree must not escape it).
    let root = std::fs::canonicalize(&root)
        .map_err(|e| format!("cannot resolve execution tree {}: {e}", root.display()))?;
    let file = root.join(entry);
    if !file.is_file() {
        return Err(format!("entry module not found in artifact tree: {entry}"));
    }

    let permissions = permissions_from_dsl(perm_dsl.unwrap_or(""))?;
    // The Deno cache is trusted input (cached remote/JS is loaded as code);
    // require an absolute location before any module can be served from it.
    resolver::validate_deno_dir()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to build tokio runtime: {e}"))?;

    rt.block_on(async {
        let url = ModuleSpecifier::from_file_path(&file)
            .map_err(|_| format!("failed to derive file url for {entry}"))?;
        let roots = node_services::ExecutionRoots {
            root: Some(root.clone()),
        };
        let (node_services, ext_services) =
            node_services::NodeServices::new(roots, permissions.clone());

        // Deno's workspace resolver owns the import map and the
        // package.json/workspace mapping. Discovery and construction are
        // deterministic and offline.
        let workspace_directory = WorkspaceDirectory::discover(
            &RealSys,
            WorkspaceDiscoverStart::Paths(std::slice::from_ref(&root)),
            &WorkspaceDiscoverOptions {
                // Read package.json too, so `workspaces` members (and their
                // own `deno.json` import maps) are discovered.
                discover_pkg_json: true,
                ..Default::default()
            },
        )
        .map_err(|e| format!("failed to discover workspace config: {e}"))?;
        let workspace = WorkspaceResolver::from_workspace(
            &workspace_directory.workspace,
            RealSys,
            CreateResolverOptions {
                // inka resolves npm from the execution tree's `node_modules`
                // (BYONM): don't map package.json deps to registry requests.
                pkg_json_dep_resolution:
                    deno_resolver::workspace::PackageJsonDepResolution::Disabled,
                ..Default::default()
            },
        )
        .map_err(|e| format!("invalid deno.json/import map: {e}"))?;
        let import_map = workspace.maybe_import_map().cloned();

        let resolver_state = resolver::build(&root, &file, import_map).await;
        let loader: Rc<dyn ModuleLoader> = Rc::new(PkgLoader {
            artifact_root: root.clone(),
            permissions: permissions.clone(),
            node_services,
            resolver: Rc::new(resolver_state),
            workspace: Rc::new(workspace),
            tsconfig: tsconfig::Resolver::new(root.clone()),
        });
        run_module_async(&url, args, permissions, loader, ext_services).await
    })
}

/// Runs an entry module from a staged multi-file artifact tree (`dir`/`entry`),
/// resolving relative imports and packages via `PkgLoader`.
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
        CString::new(format!("inka_runtime-{RUNTIME_VERSION}")).expect("nul in version string")
    })
}

#[no_mangle]
pub extern "C" fn inka_runtime_version() -> *const c_char {
    std::panic::catch_unwind(|| version_cstr().as_ptr()).unwrap_or(std::ptr::null())
}

// ---- capabilities ----------------------------------------------------------

/// Capability names this runtime supports. The requireable names come from
/// `inka-format` (single source shared with `build`/the launcher); `free-string`
/// is a runtime-only capability (the optional `inka_runtime_free_string`).
fn runtime_features_string() -> String {
    let mut names = inka_format::known_features();
    names.push("free-string");
    names.join(",")
}

fn features_cstr() -> &'static CStr {
    static V: OnceLock<CString> = OnceLock::new();
    V.get_or_init(|| CString::new(runtime_features_string()).expect("nul in features"))
}

/// The set of capabilities this runtime supports, comma-separated. Optional:
/// a launcher from before this symbol exists ignores its absence and falls back
/// to the manifest's version floor.
#[no_mangle]
pub extern "C" fn inka_runtime_features() -> *const c_char {
    std::panic::catch_unwind(|| features_cstr().as_ptr()).unwrap_or(std::ptr::null())
}

// ---- handle ----------------------------------------------------------------

#[no_mangle]
pub extern "C" fn inka_runtime_create() -> *mut c_void {
    std::panic::catch_unwind(|| Box::into_raw(Box::new(())) as *mut c_void)
        .unwrap_or(std::ptr::null_mut())
}

/// Destroy a runtime handle returned by `inka_runtime_create`.
///
/// # Safety
/// `rt` must be null or a pointer previously returned by
/// `inka_runtime_create` and not already destroyed.
#[no_mangle]
pub unsafe extern "C" fn inka_runtime_destroy(rt: *mut c_void) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if !rt.is_null() {
            drop(Box::from_raw(rt as *mut ()));
        }
    }));
}

/// Free a string the runtime handed back through `err_msg` (allocated by the
/// runtime's allocator, so it must be freed here, not by the caller).
///
/// # Safety
/// `ptr` must be null or a pointer previously returned by the runtime as an
/// `err_msg` string and not already freed.
#[no_mangle]
pub unsafe extern "C" fn inka_runtime_free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(CString::from_raw(ptr));
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

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        format!("runtime panicked: {s}")
    } else if let Some(s) = payload.downcast_ref::<String>() {
        format!("runtime panicked: {s}")
    } else {
        "runtime panicked".to_string()
    }
}

/// Run an exported ABI body, turning a panic into an error code + message
/// instead of unwinding across the C boundary (or aborting the host).
unsafe fn abi_guard<F>(err_msg: *mut *mut c_char, body: F) -> c_int
where
    F: FnOnce() -> c_int,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(rc) => rc,
        Err(payload) => {
            set_err_msg(err_msg, panic_message(payload.as_ref()));
            1
        }
    }
}

/// Run entry point: executes `entry` (a path relative to the extracted
/// `dir_path`) from a staged artifact tree, resolving its relative imports.
/// `perms` is a newline-joined string of manifest permission lines (or
/// null/empty for deny-by-default).
///
/// # Safety
/// All pointers must be null or valid C strings for the duration of the call;
/// `exit_code`/`err_msg` must be valid writable pointers.
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
    abi_guard(err_msg, || unsafe {
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use deno_runtime::deno_core::url::Url;
    use deno_runtime::deno_permissions::CheckSpecifierKind;

    fn import_allowed(dsl: &str) -> bool {
        let perms = permissions_from_dsl(dsl).expect("valid dsl");
        let url = Url::parse("https://example.com/mod.js").unwrap();
        perms
            .check_specifier(&url, CheckSpecifierKind::Static)
            .is_ok()
    }

    #[test]
    fn ts_rewrite_candidates_map_script_extensions() {
        let exts = |p: &str| {
            ts_rewrite_candidates(Path::new(p))
                .into_iter()
                .map(|c| c.extension().unwrap().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(exts("src/index.js"), vec!["ts", "tsx"]);
        assert_eq!(exts("src/index.jsx"), vec!["tsx", "ts"]);
        assert_eq!(exts("src/index.mjs"), vec!["mts"]);
        assert_eq!(exts("src/index.cjs"), vec!["cts"]);
        // Non-script extensions and extensionless paths are untouched.
        assert!(ts_rewrite_candidates(Path::new("src/index.ts")).is_empty());
        assert!(ts_rewrite_candidates(Path::new("src/index.json")).is_empty());
        assert!(ts_rewrite_candidates(Path::new("src/index")).is_empty());
    }

    #[test]
    fn comma_lists_preserve_spaces() {
        let spec = parse_perm_dsl("allow-read=./My Data,/etc").unwrap();
        assert_eq!(
            find(&spec.allow, "read").unwrap(),
            &vec!["./My Data".to_string(), "/etc".to_string()]
        );
    }

    #[test]
    fn empty_dsl_denies_import() {
        assert!(!import_allowed(""));
        assert!(!import_allowed("permissions=none"));
    }

    // `permissions=all` (and `all` trimmed by a deny) must allow every kind,
    // matching `Permissions::allow_all()` — including `import`, which is not
    // part of the manifest DSL.
    #[test]
    fn all_with_deny_still_allows_import() {
        assert!(import_allowed("permissions=all"));
        assert!(import_allowed("permissions=all\ndeny-read=./secrets"));
    }

    #[test]
    fn deny_without_allow_is_deny_by_default() {
        // A lone deny cannot grant anything.
        assert!(!import_allowed("deny-read=./secrets"));
    }

    #[test]
    fn empty_allow_list_is_rejected() {
        assert!(parse_perm_dsl("allow-read=").is_err());
        assert!(parse_perm_dsl("allow-read=,,").is_err());
        assert!(parse_perm_dsl("allow-read=   ").is_err());
        // `*` is the explicit "all in this category" form.
        assert!(parse_perm_dsl("allow-read=*").is_ok());
    }

    #[test]
    fn allow_import_grants_import_only() {
        assert!(import_allowed("allow-import=*"));
        // A non-import allow does not grant import.
        assert!(!import_allowed("allow-read=/etc"));
        // `permissions=all` trimmed by deny-import denies import.
        assert!(!import_allowed("permissions=all\ndeny-import=*"));
    }

    #[test]
    fn advertises_every_requireable_feature() {
        let advertised = runtime_features_string();
        for f in inka_format::known_features() {
            assert!(
                advertised.split(',').any(|x| x == f),
                "runtime must advertise '{f}': {advertised}"
            );
        }
        assert!(advertised.split(',').any(|x| x == "free-string"));
    }
}

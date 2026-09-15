//! Offline resolution for the bundler.
//!
//! Builds a `deno_graph::ModuleGraph` over the entry using only the local Deno
//! cache (`$DENO_DIR`): the workspace import map (resolved by
//! `deno_resolver`) maps bare specifiers, and `jsr:` (plus remote `https:`)
//! modules are read from `$DENO_DIR/remote`. `npm:` is resolved later by
//! rolldown against `node_modules`.
//!
//! The graph is reduced to `Send + Sync` lookup maps so it can live inside the
//! rolldown plugin.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use deno_cache_dir::{GlobalHttpCache, HttpCache};
use deno_config::workspace::{
    WorkspaceDirectory, WorkspaceDiscoverOptions, WorkspaceDiscoverStart,
};
use deno_graph::source::{
    LoadFuture, LoadOptions, LoadResponse, Loader, ResolutionKind, Resolver as GraphResolver,
};
use deno_graph::{BuildOptions, GraphKind, ModuleGraph, ModuleSpecifier, Range, Resolution};
use deno_resolver::workspace::{CreateResolverOptions, WorkspaceResolver};
use sys_traits::impls::RealSys;
use url::Url;

fn load_error(msg: String) -> deno_graph::source::LoadError {
    deno_graph::source::LoadError::Other(Arc::new(deno_error::JsErrorBox::generic(msg)))
}

/// Cache-only loader: `file:` from disk, `http(s):` from `$DENO_DIR/remote`.
struct CacheLoader {
    cache: Arc<GlobalHttpCache<RealSys>>,
}

impl Loader for CacheLoader {
    fn load(&self, specifier: &ModuleSpecifier, _options: LoadOptions) -> LoadFuture {
        let spec = specifier.clone();
        let cache = self.cache.clone();
        Box::pin(async move {
            match spec.scheme() {
                "file" => {
                    let path = spec
                        .to_file_path()
                        .map_err(|_| load_error(format!("not a file url: {spec}")))?;
                    let bytes = std::fs::read(&path)
                        .map_err(|e| load_error(format!("read {}: {e}", path.display())))?;
                    Ok(Some(LoadResponse::Module {
                        content: bytes.into(),
                        mtime: None,
                        specifier: spec,
                        maybe_headers: None,
                    }))
                }
                "http" | "https" => {
                    let key = cache
                        .cache_item_key(&spec)
                        .map_err(|e| load_error(format!("cache key: {e}")))?;
                    match cache
                        .get(&key, None)
                        .map_err(|e| load_error(format!("cache get: {e}")))?
                    {
                        Some(entry) => Ok(Some(LoadResponse::Module {
                            content: entry.content.into_owned().into(),
                            mtime: None,
                            specifier: spec,
                            maybe_headers: Some(entry.metadata.headers.clone()),
                        })),
                        None => Err(load_error(format!(
                            "module not in the Deno cache (offline): {spec}"
                        ))),
                    }
                }
                _ => Ok(Some(LoadResponse::External { specifier: spec })),
            }
        })
    }
}

struct ImportMapGraphResolver {
    map: import_map::ImportMap,
}

impl std::fmt::Debug for ImportMapGraphResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ImportMapGraphResolver")
    }
}

impl GraphResolver for ImportMapGraphResolver {
    fn resolve(
        &self,
        specifier_text: &str,
        referrer_range: &Range,
        _kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, deno_graph::source::ResolveError> {
        if let Ok(u) = self.map.resolve(specifier_text, &referrer_range.specifier) {
            return Ok(u);
        }
        Ok(deno_graph::resolve_import(
            specifier_text,
            &referrer_range.specifier,
        )?)
    }
}

/// Send + Sync resolution data extracted from the graph.
pub(crate) struct ResolverState {
    /// `(referrer, requested specifier)` -> resolved specifier.
    pub edges: HashMap<(String, String), String>,
    /// `jsr:…` specifier -> resolved `https://jsr.io/…` URL.
    pub jsr_redirects: HashMap<String, String>,
    pub import_map: Option<import_map::ImportMap>,
    pub deno_dir: PathBuf,
    /// Absolute resolved module ids recorded by the rolldown plugin (`npm:` and
    /// bare packages resolved through `ctx.resolve`). Used post-build to locate
    /// packages that carry run-time asset files (e.g. TypeScript's libs).
    pub resolved_files: Arc<Mutex<Vec<String>>>,
}

impl ResolverState {
    /// Apply the `deno.json` import map to a specifier.
    pub fn map_specifier(&self, specifier: &str, importer: Option<&str>) -> Option<String> {
        let map = self.import_map.as_ref()?;
        let referrer = importer.and_then(importer_url)?;
        map.resolve(specifier, &referrer)
            .ok()
            .map(|u| u.to_string())
    }

    /// Record a resolved module id (best-effort; duplicates are ignored).
    pub(crate) fn record_resolved(&self, id: &str) {
        if !id.contains("node_modules") && !id.starts_with("file://") {
            return;
        }
        if let Ok(mut v) = self.resolved_files.lock() {
            if !v.iter().any(|e| e == id) {
                v.push(id.to_string());
            }
        }
    }
}

/// Convert a rolldown module id (absolute path or URL) into a `file:` URL.
pub(crate) fn importer_url(importer: &str) -> Option<Url> {
    if let Ok(u) = Url::parse(importer) {
        return Some(u);
    }
    Url::from_file_path(importer).ok()
}

/// The Deno cache directory (`$DENO_DIR` else the platform cache dir),
/// resolved exactly as Deno resolves it.
pub(crate) fn deno_dir() -> PathBuf {
    deno_cache_dir::resolve_deno_dir(
        &RealSys,
        deno_cache_dir::ResolveDenoDirOptions {
            maybe_initial_cwd: None,
            maybe_custom_root: None,
        },
    )
    .map(|c| c.into_owned())
    .unwrap_or_else(|_| PathBuf::from(".deno"))
}

/// Build the resolver state for `entry` under `root`.
pub(crate) async fn build(root: &Path, entry: &Path) -> ResolverState {
    let deno_dir = deno_dir();
    let import_map = workspace_import_map(root);
    let mut state = ResolverState {
        edges: HashMap::new(),
        jsr_redirects: HashMap::new(),
        import_map,
        deno_dir: deno_dir.clone(),
        resolved_files: Arc::new(Mutex::new(Vec::new())),
    };

    let Some(import_map) = state.import_map.clone() else {
        return state;
    };
    if !deno_dir.is_absolute() {
        return state;
    }

    let cache = Arc::new(GlobalHttpCache::new(RealSys, deno_dir.join("remote")));
    let loader = CacheLoader { cache };
    let Ok(entry_url) = Url::from_file_path(entry) else {
        return state;
    };
    let resolver = ImportMapGraphResolver { map: import_map };
    let mut graph = ModuleGraph::new(GraphKind::All);
    graph
        .build(
            vec![entry_url],
            vec![],
            &loader,
            BuildOptions {
                prefer_cached_jsr_versions: true,
                resolver: Some(&resolver),
                ..Default::default()
            },
        )
        .await;

    for module in graph.modules() {
        let referrer = module.specifier().to_string();
        for (requested, dep) in module.dependencies() {
            if let Resolution::Ok(resolved) = &dep.maybe_code {
                state.edges.insert(
                    (referrer.clone(), requested.clone()),
                    resolved.specifier.to_string(),
                );
            }
        }
    }

    // Follow jsr: -> https: redirects for every jsr specifier seen.
    let mut jsr_specs: BTreeSet<String> = BTreeSet::new();
    for ((_referrer, requested), resolved) in &state.edges {
        if requested.starts_with("jsr:") {
            jsr_specs.insert(requested.clone());
        }
        if resolved.starts_with("jsr:") {
            jsr_specs.insert(resolved.clone());
        }
    }
    for spec in jsr_specs {
        if let Ok(u) = Url::parse(&spec) {
            if let Some(module) = graph.get(&u) {
                state
                    .jsr_redirects
                    .insert(spec, module.specifier().to_string());
            }
        }
    }

    state
}

/// The workspace import map (workspace root plus member `deno.json` import
/// maps), resolved by Deno's resolver — the same map `inka run` uses, so build
/// and run agree.
fn workspace_import_map(root: &Path) -> Option<import_map::ImportMap> {
    let root_buf = root.to_path_buf();
    let workspace_directory = WorkspaceDirectory::discover(
        &RealSys,
        WorkspaceDiscoverStart::Paths(std::slice::from_ref(&root_buf)),
        &WorkspaceDiscoverOptions {
            discover_pkg_json: true,
            ..Default::default()
        },
    )
    .ok()?;
    let workspace = WorkspaceResolver::from_workspace(
        &workspace_directory.workspace,
        RealSys,
        CreateResolverOptions::default(),
    )
    .ok()?;
    workspace.maybe_import_map().cloned()
}

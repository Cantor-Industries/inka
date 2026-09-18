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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use deno_cache_dir::{GlobalHttpCache, HeadersMap, HttpCache};
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
/// When `online`, a remote cache miss is fetched (opt-in `inka cache`/`--fetch`)
/// and written to the cache; the default stays fully offline.
struct CacheLoader {
    cache: Arc<GlobalHttpCache<RealSys>>,
    online: bool,
    /// Count of remote modules actually fetched (for user feedback).
    fetched: Arc<AtomicUsize>,
}

impl Loader for CacheLoader {
    fn load(&self, specifier: &ModuleSpecifier, _options: LoadOptions) -> LoadFuture {
        let spec = specifier.clone();
        let cache = self.cache.clone();
        let online = self.online;
        let fetched = self.fetched.clone();
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
                    if let Some(entry) = cache
                        .get(&key, None)
                        .map_err(|e| load_error(format!("cache get: {e}")))?
                    {
                        return Ok(Some(LoadResponse::Module {
                            content: entry.content.into_owned().into(),
                            mtime: None,
                            specifier: spec,
                            maybe_headers: Some(entry.metadata.headers.clone()),
                        }));
                    }
                    if online {
                        let resp = fetch_remote(&spec, &cache).await?;
                        if resp.is_some() {
                            fetched.fetch_add(1, Ordering::Relaxed);
                        }
                        return Ok(resp);
                    }
                    Err(load_error(format!(
                        "module not in the Deno cache (offline): {spec}"
                    )))
                }
                _ => Ok(Some(LoadResponse::External { specifier: spec })),
            }
        })
    }
}

/// Fetch a remote module over the network (opt-in) and write it to the Deno
/// cache. Redirects are surfaced to `deno_graph` (auto-follow is disabled) so
/// its specifier tracking stays correct. Blocking `ureq` is fine here: the
/// resolver runs on a dedicated current-thread runtime.
async fn fetch_remote(
    spec: &ModuleSpecifier,
    cache: &GlobalHttpCache<RealSys>,
) -> Result<Option<LoadResponse>, deno_graph::source::LoadError> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .max_redirects(0)
            .user_agent(concat!("inka/", env!("CARGO_PKG_VERSION")))
            .build(),
    );
    let resp = agent
        .get(spec.as_str())
        .call()
        .map_err(|e| load_error(format!("fetch failed for {spec}: {e}")))?;
    let status = resp.status();
    if status.is_redirection() {
        if let Some(loc) = resp
            .headers()
            .get(ureq::http::header::LOCATION)
            .and_then(|v| v.to_str().ok())
        {
            let target = spec
                .join(loc)
                .map_err(|e| load_error(format!("invalid redirect from {spec}: {e}")))?;
            return Ok(Some(LoadResponse::Redirect { specifier: target }));
        }
    }
    if !status.is_success() {
        return Err(load_error(format!("HTTP {} for {spec}", status.as_u16())));
    }
    let headers: HeadersMap = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let body = resp
        .into_body()
        .read_to_vec()
        .map_err(|e| load_error(format!("reading {spec}: {e}")))?;
    cache
        .set(spec, headers.clone(), &body)
        .map_err(|e| load_error(format!("cache write for {spec}: {e}")))?;
    Ok(Some(LoadResponse::Module {
        content: body.into(),
        mtime: None,
        specifier: spec.clone(),
        maybe_headers: Some(headers),
    }))
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

/// Build the offline resolver state for `entry` under `root`. When `online`, a
/// remote cache miss is fetched and cached (opt-in).
pub(crate) async fn build(root: &Path, entry: &Path, online: bool) -> ResolverState {
    let deno_dir = deno_dir();
    let import_map = workspace_import_map(root);
    let mut state = ResolverState {
        edges: HashMap::new(),
        jsr_redirects: HashMap::new(),
        import_map,
        deno_dir: deno_dir.clone(),
    };

    let Some(import_map) = state.import_map.clone() else {
        if online {
            // No import map: still allow direct `jsr:`/`https:` specifiers to be
            // fetched (the graph resolves schemes itself).
            let _ = build_graph(entry, true, None, &deno_dir).await;
        }
        return state;
    };
    if !deno_dir.is_absolute() {
        return state;
    }

    let (graph, _) = build_graph(entry, online, Some(&import_map), &deno_dir).await;

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

/// Build a `deno_graph` for `entry`, reading from the Deno cache and (when
/// `online`) fetching missing remote modules into it.
async fn build_graph(
    entry: &Path,
    online: bool,
    import_map: Option<&import_map::ImportMap>,
    deno_dir: &Path,
) -> (ModuleGraph, usize) {
    let cache = Arc::new(GlobalHttpCache::new(RealSys, deno_dir.join("remote")));
    let fetched = Arc::new(AtomicUsize::new(0));
    let loader = CacheLoader {
        cache,
        online,
        fetched: fetched.clone(),
    };
    let Ok(entry_url) = Url::from_file_path(entry) else {
        return (ModuleGraph::new(GraphKind::All), 0);
    };
    let resolver = import_map
        .cloned()
        .map(|map| ImportMapGraphResolver { map });
    let mut graph = ModuleGraph::new(GraphKind::All);
    graph
        .build(
            vec![entry_url],
            vec![],
            &loader,
            BuildOptions {
                prefer_cached_jsr_versions: true,
                resolver: resolver.as_ref().map(|r| r as &dyn GraphResolver),
                ..Default::default()
            },
        )
        .await;
    (graph, fetched.load(Ordering::Relaxed))
}

/// Opt-in cache warming: fetch every remote (`jsr:`/`https:`) module in
/// `entry`'s graph that is missing from the Deno cache, so later builds/runs
/// work offline. Requires an absolute `DENO_DIR`. Returns the number of remote
/// modules fetched.
pub async fn warm_cache(root: &Path, entry: &Path) -> Result<usize, String> {
    let deno_dir = deno_dir();
    if !deno_dir.is_absolute() {
        return Err("DENO_DIR must be an absolute path to fetch remote modules".to_string());
    }
    let import_map = workspace_import_map(root);
    let (_, fetched) = build_graph(entry, true, import_map.as_ref(), &deno_dir).await;
    Ok(fetched)
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

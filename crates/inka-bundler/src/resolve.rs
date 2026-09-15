//! Offline resolution for the bundler.
//!
//! Builds a `deno_graph::ModuleGraph` over the entry using only the local Deno
//! cache (`$DENO_DIR`): a `deno.json` import map maps bare specifiers, and
//! `jsr:` (plus remote `https:`) modules are read from `$DENO_DIR/remote`.
//! `npm:` is resolved later by rolldown against `node_modules`.
//!
//! The graph is reduced to `Send + Sync` lookup maps so it can live inside the
//! rolldown plugin.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use deno_cache_dir::{GlobalHttpCache, HttpCache};
use deno_graph::source::{
    LoadFuture, LoadOptions, LoadResponse, Loader, ResolutionKind, Resolver as GraphResolver,
};
use deno_graph::{BuildOptions, GraphKind, ModuleGraph, ModuleSpecifier, Range, Resolution};
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

/// Build the resolver state for `entry` under `root`.
pub(crate) async fn build(root: &Path, entry: &Path) -> ResolverState {
    let deno_dir = deno_dir();
    let deno_json = ["deno.json", "deno.jsonc"]
        .iter()
        .map(|f| root.join(f))
        .find(|p| p.is_file());

    let import_map = deno_json.as_deref().and_then(|p| load_import_map(p).ok());
    let mut state = ResolverState {
        edges: HashMap::new(),
        jsr_redirects: HashMap::new(),
        import_map,
        deno_dir: deno_dir.clone(),
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

fn load_import_map(path: &Path) -> Result<import_map::ImportMap, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    // `deno.jsonc` allows comments/trailing commas; parse it as JSONC.
    let value: serde_json::Value = jsonc_parser::parse_to_serde_value(&text, &Default::default())
        .map_err(|e| e.to_string())?;
    let mut map_value = serde_json::Map::new();
    if let Some(v) = value.get("imports") {
        map_value.insert("imports".to_string(), v.clone());
    }
    if let Some(v) = value.get("scopes") {
        map_value.insert("scopes".to_string(), v.clone());
    }
    let base = Url::from_file_path(path).map_err(|_| "bad config path".to_string())?;
    Ok(
        import_map::parse_from_value(base, serde_json::Value::Object(map_value))
            .map_err(|e| e.to_string())?
            .import_map,
    )
}

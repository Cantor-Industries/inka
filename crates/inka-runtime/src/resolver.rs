//! Offline module-graph resolution for `inka run` (and cached modules).
//!
//! Builds a `deno_graph::ModuleGraph` for the execution entry using only the
//! local Deno cache (`$DENO_DIR`): a `deno.json` import map maps bare
//! specifiers, and `jsr:` (plus any remote `https:`) modules are read from
//! `$DENO_DIR/remote`. `npm:` is resolved later against the execution tree's
//! `node_modules`. No network is ever used.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use deno_cache_dir::{GlobalHttpCache, HttpCache};
use deno_graph::source::{
    LoadFuture, LoadOptions, LoadResponse, Loader, ResolutionKind, Resolver as GraphResolver,
};
use deno_graph::{BuildOptions, GraphKind, ModuleGraph, ModuleSpecifier, Range};
use sys_traits::impls::RealSys;
use url::Url;

fn load_error(msg: String) -> deno_graph::source::LoadError {
    deno_graph::source::LoadError::Other(Arc::new(deno_error::JsErrorBox::generic(msg)))
}

/// Cache-only loader: `file:` from disk (confined to `root`), `http(s):` from
/// `$DENO_DIR/remote`, everything else external.
struct CacheLoader {
    cache: Arc<GlobalHttpCache<RealSys>>,
    /// Canonical execution root; `file:` loads outside it are refused.
    root: PathBuf,
}

impl Loader for CacheLoader {
    fn load(&self, specifier: &ModuleSpecifier, _options: LoadOptions) -> LoadFuture {
        let spec = specifier.clone();
        let cache = self.cache.clone();
        let root = self.root.clone();
        Box::pin(async move {
            match spec.scheme() {
                "file" => {
                    let path = spec
                        .to_file_path()
                        .map_err(|_| load_error(format!("not a file url: {spec}")))?;
                    // Resolve symlinks and confine to the execution tree (the
                    // import map could otherwise point a bare specifier at any
                    // local file).
                    let real = std::fs::canonicalize(&path)
                        .map_err(|e| load_error(format!("read {}: {e}", path.display())))?;
                    if !real.starts_with(&root) {
                        return Err(load_error(format!(
                            "refusing to load module outside the execution tree: {spec}"
                        )));
                    }
                    let bytes = std::fs::read(&real)
                        .map_err(|e| load_error(format!("read {}: {e}", real.display())))?;
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

/// deno_graph resolver backed by the `deno.json` import map.
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

/// A pre-built module graph plus import map, used for synchronous resolution at
/// module-load time.
pub(crate) struct GraphResolverState {
    graph: ModuleGraph,
    import_map: Option<import_map::ImportMap>,
    deno_dir: PathBuf,
}

impl GraphResolverState {
    pub(crate) fn empty() -> Self {
        Self {
            graph: ModuleGraph::new(GraphKind::All),
            import_map: None,
            deno_dir: deno_dir_path(),
        }
    }

    pub(crate) fn deno_dir(&self) -> &Path {
        &self.deno_dir
    }

    /// Apply the `deno.json` import map to a specifier (if one is configured).
    pub(crate) fn map_specifier(&self, specifier: &str, referrer: &str) -> Option<String> {
        let map = self.import_map.as_ref()?;
        let referrer = Url::parse(referrer).ok()?;
        map.resolve(specifier, &referrer)
            .ok()
            .map(|u| u.to_string())
    }

    /// Resolve a specifier against the graph, following `jsr:` → `https:`
    /// redirects (via `ModuleGraph::get`).
    pub(crate) fn resolve_in_graph(&self, specifier: &str, referrer: &str) -> Option<Url> {
        let target = deno_graph::resolve_import(specifier, &Url::parse(referrer).ok()?).ok()?;
        if let Some(module) = self.graph.get(&target) {
            return Some(module.specifier().clone());
        }
        Some(target)
    }
}

/// The Deno cache directory: `$DENO_DIR` or `~/.cache/deno`. This may be
/// relative when `DENO_DIR`/`HOME` are relative; callers that read the cache
/// must go through `validate_deno_dir` first.
fn raw_deno_dir() -> PathBuf {
    if let Ok(d) = std::env::var("DENO_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".cache/deno")
}

pub(crate) fn deno_dir_path() -> PathBuf {
    raw_deno_dir()
}

/// Require an absolute cache directory. The Deno cache is trusted input (cached
/// remote/JS is loaded as code), so a relative or otherwise ambiguous location
/// is refused rather than resolved against the launch directory.
pub(crate) fn validate_deno_dir() -> Result<PathBuf, String> {
    let dir = raw_deno_dir();
    if !dir.is_absolute() {
        return Err(format!(
            "DENO_DIR must be an absolute path (got '{}'); the Deno cache is trusted input",
            dir.display()
        ));
    }
    Ok(dir)
}

/// Build the graph for `entry` under `root`. Returns an empty state when there
/// is no `deno.json`/`deno.jsonc` (nothing to map) or on any error.
pub(crate) async fn build(root: &Path, entry: &Path) -> GraphResolverState {
    let deno_json = ["deno.json", "deno.jsonc"]
        .iter()
        .map(|f| root.join(f))
        .find(|p| p.is_file());
    let Some(deno_json) = deno_json else {
        return GraphResolverState::empty();
    };

    let import_map = match load_import_map(&deno_json) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "[inka] warning: ignoring import map from {}: {e}",
                deno_json.display()
            );
            return GraphResolverState::empty();
        }
    };

    let deno_dir = match validate_deno_dir() {
        Ok(d) => d,
        Err(_) => return GraphResolverState::empty(),
    };
    let cache = Arc::new(GlobalHttpCache::new(RealSys, deno_dir.join("remote")));
    let loader = CacheLoader {
        cache,
        root: root.to_path_buf(),
    };

    let Ok(entry_url) = Url::from_file_path(entry) else {
        return GraphResolverState::empty();
    };
    let resolver = ImportMapGraphResolver {
        map: import_map.clone(),
    };
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

    GraphResolverState {
        graph,
        import_map: Some(import_map),
        deno_dir,
    }
}

fn load_import_map(path: &Path) -> Result<import_map::ImportMap, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    // Keep only the import-map keys; deno.json carries unrelated config too.
    let mut map_value = serde_json::Map::new();
    if let Some(v) = value.get("imports") {
        map_value.insert("imports".to_string(), v.clone());
    }
    if let Some(v) = value.get("scopes") {
        map_value.insert("scopes".to_string(), v.clone());
    }
    let base = Url::from_file_path(path).map_err(|_| "bad config path".to_string())?;
    let map = import_map::parse_from_value(base, serde_json::Value::Object(map_value))
        .map_err(|e| e.to_string())?
        .import_map;
    Ok(map)
}

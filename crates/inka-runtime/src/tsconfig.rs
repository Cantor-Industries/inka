//! Run-time `tsconfig.json`/`jsconfig.json` `baseUrl`/`paths` resolution.
//!
//! `inka build` resolves such aliases through rolldown's `oxc_resolver`
//! (`TsConfig::Auto`). To keep `inka run` in lockstep, this module drives the
//! same crate: a bare specifier is resolved against the *importing file's*
//! nearest tsconfig, and the result is confined to the execution tree so a
//! tsconfig can never widen module reads.
//!
//! This is layered after the workspace resolver (import map / `#imports` /
//! workspace members) and before Deno's node/`node_modules` resolution, matching
//! both TypeScript's precedence and `inka build`.

use std::path::PathBuf;

use oxc_resolver::{ResolveOptions, Resolver as OxcResolver, TsconfigDiscovery};
use url::Url;

/// Per-referrer tsconfig resolver (caches tsconfig discovery internally).
pub(crate) struct Resolver {
    /// Canonical execution root; resolved files must stay under it.
    root: PathBuf,
    inner: OxcResolver,
}

impl Resolver {
    pub(crate) fn new(root: PathBuf) -> Self {
        // tsconfig-only: `modules` is empty so npm stays with Deno's node
        // resolver. Extensions/extension aliases mirror `rolldown_resolver`,
        // so build and run pick the same file.
        let inner = OxcResolver::new(ResolveOptions {
            cwd: Some(root.clone()),
            tsconfig: Some(TsconfigDiscovery::Auto),
            modules: Vec::new(),
            extensions: [".tsx", ".ts", ".jsx", ".js", ".json"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            extension_alias: vec![
                (
                    ".js".into(),
                    vec![".js".into(), ".ts".into(), ".tsx".into()],
                ),
                (
                    ".jsx".into(),
                    vec![".jsx".into(), ".ts".into(), ".tsx".into()],
                ),
                (".mjs".into(), vec![".mjs".into(), ".mts".into()]),
                (".cjs".into(), vec![".cjs".into(), ".cts".into()]),
            ],
            ..Default::default()
        });
        Self { root, inner }
    }

    /// Resolve a bare `specifier` imported from `referrer` via the nearest
    /// tsconfig `baseUrl`/`paths`. Returns `None` when no config applies, the
    /// specifier does not map, or the target is missing or outside the tree.
    pub(crate) fn resolve(&self, specifier: &str, referrer: &Url) -> Option<Url> {
        let ref_path = referrer.to_file_path().ok()?;
        let resolved = self.inner.resolve_file(&ref_path, specifier).ok()?;
        let real = std::fs::canonicalize(resolved.path()).ok()?;
        if !real.starts_with(&self.root) {
            return None;
        }
        Url::from_file_path(&real).ok()
    }
}

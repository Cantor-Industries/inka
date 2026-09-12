// inka runtime: the single seam between inka and Deno's Node/CJS machinery.
//
// Everything that names a `deno_node` / `node_resolver` / `deno_core` type
// lives here so that a Deno runtime tuple bump is a contained edit. The rest of
// the engine (`lib.rs`, `build.rs`) must not reference those crates directly.
//
// Tuple-coupled touchpoints (revisit on every `deno_runtime` bump):
//   - `NodeRequireLoader` / `NpmPackageFolderResolver` / `InNpmPackageChecker`
//     trait signatures,
//   - `NodeExtInitServices` field set,
//   - `NodeResolver::new` + `NodeResolverOptions`,
//   - `PackageJsonResolver::new` / `NodeResolutionSys::new`,
//   - `CjsCodeAnalyzer` / `CjsModuleExportAnalyzer` / `NodeCodeTranslator`,
//   - the `deno_node` re-export path (`deno_runtime::deno_node`).
//
// Policy (which files, vendored -> store -> builtins precedence, ESM vs CJS
// classification) is deliberately kept in `inka-resolver` where possible; this
// module is translation only.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use deno_error::JsErrorBox;
use deno_runtime::deno_core::url::Url;
use deno_runtime::deno_core::FastString;
use deno_runtime::deno_fs::sync::new_rc;
use deno_runtime::deno_node::{
    NodeExtInitServices, NodeRequireLoader, NodeRequireLoaderRc, NodeResolver, NodeResolverRc,
};
use deno_runtime::deno_permissions::{OpenAccessKind, PermissionsContainer};
use node_resolver::analyze::{
    CjsAnalysis, CjsAnalysisExports, CjsCodeAnalyzer, CjsModuleExportAnalyzer,
    CjsModuleExportAnalyzerRc, EsmAnalysisMode, NodeCodeTranslator, NodeCodeTranslatorMode,
    NodeCodeTranslatorRc, ResolvedCjsAnalysis,
};
use node_resolver::cache::NodeResolutionSys;
use node_resolver::errors::{
    PackageFolderResolveError, PackageFolderResolveErrorKind, PackageJsonLoadError,
    PackageNotFoundError,
};
use node_resolver::{
    DenoIsBuiltInNodeModuleChecker, InNpmPackageChecker, NodeConditionOptions, NodeResolverOptions,
    NpmPackageFolderResolver, PackageJsonResolver, PackageJsonResolverRc, UrlOrPathRef,
};
use sys_traits::impls::RealSys;

/// Filesystem roots a `require()` is allowed to reach without an explicit read
/// grant: the shared store, the artifact's embedded `vendored/` tree, and the
/// artifact tree itself. Reads outside these roots stay deny-by-default.
#[derive(Clone, Default)]
pub(crate) struct StoreRoots {
    pub store: Option<PathBuf>,
    pub vendor: Option<PathBuf>,
    pub artifact: Option<PathBuf>,
}

impl StoreRoots {
    fn contains(&self, path: &Path) -> bool {
        [&self.store, &self.vendor, &self.artifact]
            .into_iter()
            .flatten()
            .any(|root| path.starts_with(root))
    }

    /// Under the store or the vendored package roots (node_modules-like, where
    /// a `.js` without an explicit `"type"` defaults to CommonJS).
    fn in_package_root(&self, path: &Path) -> bool {
        [&self.store, &self.vendor]
            .into_iter()
            .flatten()
            .any(|root| path.starts_with(root))
    }
}

/// The npm package name from a bare specifier (`@scope/name` or `name`).
fn package_name(spec: &str) -> String {
    if let Some(rest) = spec.strip_prefix('@') {
        match rest.split_once('/') {
            Some((scope, tail)) => {
                let name = tail.split('/').next().unwrap_or(tail);
                format!("@{scope}/{name}")
            }
            None => spec.to_string(),
        }
    } else {
        spec.split('/').next().unwrap_or(spec).to_string()
    }
}

fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// `require()` semantics: a `.js` without `"type": "module"` is CJS.
fn require_is_maybe_cjs(pkg_json: &PackageJsonResolver<RealSys>, path: &Path) -> bool {
    match extension(path).as_deref() {
        Some("cjs" | "cts") => true,
        Some("mjs" | "mts" | "json") => false,
        _ => pkg_json
            .get_closest_package_json(path)
            .ok()
            .flatten()
            .map(|pkg| pkg.typ != "module")
            .unwrap_or(true),
    }
}

#[derive(Clone)]
pub(crate) struct StoreNpmChecker {
    roots: StoreRoots,
}

impl InNpmPackageChecker for StoreNpmChecker {
    fn in_npm_package(&self, specifier: &Url) -> bool {
        specifier
            .to_file_path()
            .map(|p| self.roots.contains(&p))
            .unwrap_or(false)
    }
}

#[derive(Clone)]
pub(crate) struct StoreFolderResolver {
    roots: StoreRoots,
}

impl NpmPackageFolderResolver for StoreFolderResolver {
    fn resolve_package_folder_from_package(
        &self,
        specifier: &str,
        referrer: &UrlOrPathRef,
    ) -> Result<PathBuf, PackageFolderResolveError> {
        let name = package_name(specifier);

        // Vendored package roots shadow the store (name-keyed, no node_modules).
        if let Some(vendor) = &self.roots.vendor {
            let candidate = vendor.join(&name);
            if candidate.join("package.json").is_file() {
                return Ok(candidate);
            }
        }

        // Shared hoisted store pool.
        if let Some(store) = &self.roots.store {
            let candidate = store.join("node_modules").join(&name);
            if candidate.join("package.json").is_file() {
                return Ok(candidate);
            }
        }

        // Node-style walk up from the referrer, confined to the trusted roots
        // (covers nested node_modules inside the store/vendor trees).
        if let Ok(ref_path) = referrer.path() {
            let mut dir = ref_path.parent();
            while let Some(d) = dir {
                if !self.roots.contains(d) {
                    break;
                }
                let candidate = d.join("node_modules").join(&name);
                if candidate.join("package.json").is_file() {
                    return Ok(candidate);
                }
                dir = d.parent();
            }
        }

        Err(PackageFolderResolveError(Box::new(
            PackageFolderResolveErrorKind::PackageNotFound(PackageNotFoundError {
                package_name: name,
                referrer: referrer.display(),
                referrer_extra: None,
            }),
        )))
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

struct StoreRequireLoader {
    roots: StoreRoots,
    pkg_json: PackageJsonResolverRc<RealSys>,
}

impl NodeRequireLoader for StoreRequireLoader {
    fn ensure_read_permission<'a>(
        &self,
        permissions: &mut PermissionsContainer,
        path: Cow<'a, Path>,
    ) -> Result<Cow<'a, Path>, JsErrorBox> {
        // Reads inside the store/vendored/artifact roots are implicit (the
        // packages are trusted and the ESM loader already confines module
        // reads). Anything else is deny-by-default unless `--allow-read`
        // grants it (Deno semantics).
        if self.roots.contains(path.as_ref()) {
            return Ok(path);
        }
        let checked = permissions
            .check_open(path, OpenAccessKind::ReadNoFollow, Some("require"))
            .map_err(JsErrorBox::from_err)?;
        Ok(checked.into_path())
    }

    fn load_text_file_lossy(&self, path: &Path) -> Result<FastString, JsErrorBox> {
        let bytes = std::fs::read(path).map_err(JsErrorBox::from_err)?;
        Ok(FastString::from(
            String::from_utf8_lossy(&bytes).into_owned(),
        ))
    }

    fn is_maybe_cjs(&self, specifier: &Url) -> Result<bool, PackageJsonLoadError> {
        Ok(specifier
            .to_file_path()
            .map(|p| require_is_maybe_cjs(&self.pkg_json, &p))
            .unwrap_or(false))
    }

    fn is_maybe_cjs_from_require(&self, specifier: &Url) -> Result<bool, PackageJsonLoadError> {
        self.is_maybe_cjs(specifier)
    }
}

/// Static CJS export analysis for the ESM facade, using `deno_ast`'s
/// `cjs-module-lexer`-equivalent (`ParsedSource::analyze_cjs`). Parses the
/// source so an ESM file in a package root is passed through untouched.
struct InkaCjsCodeAnalyzer {
    roots: StoreRoots,
}

#[async_trait::async_trait(?Send)]
impl CjsCodeAnalyzer for InkaCjsCodeAnalyzer {
    async fn analyze_cjs<'a>(
        &self,
        specifier: &Url,
        maybe_source: Option<Cow<'a, str>>,
        _esm_analysis_mode: EsmAnalysisMode,
    ) -> Result<CjsAnalysis<'a>, JsErrorBox> {
        let path = specifier
            .to_file_path()
            .map_err(|_| JsErrorBox::generic(format!("not a file URL: {specifier}")))?;
        if !self.roots.contains(&path) {
            return Err(JsErrorBox::generic(format!(
                "refusing to analyze a CJS module outside the store/vendored tree: {specifier}"
            )));
        }
        let source = match maybe_source {
            Some(source) => source.into_owned(),
            None => std::fs::read_to_string(&path).map_err(JsErrorBox::from_err)?,
        };
        let media_type = deno_ast::MediaType::from_path(&path);
        let parsed = deno_ast::parse_program(deno_ast::ParseParams {
            specifier: specifier.clone(),
            text: source.clone().into(),
            media_type,
            capture_tokens: false,
            scope_analysis: false,
            maybe_syntax: None,
        })
        .map_err(|e| JsErrorBox::generic(format!("failed to parse CJS module {specifier}: {e}")))?;
        if !parsed.compute_is_script() {
            // It's an ES module; hand it back unchanged.
            return Ok(CjsAnalysis::Esm(Cow::Owned(source), None));
        }
        let analysis = parsed.analyze_cjs();
        Ok(CjsAnalysis::Cjs(CjsAnalysisExports {
            exports: analysis.exports,
            reexports: analysis.reexports,
            member_reexports: Vec::new(),
        }))
    }

    async fn analyze_cjs_member_props<'a>(
        &self,
        _specifier: &Url,
        _maybe_source: Option<Cow<'a, str>>,
        _member: &str,
    ) -> Result<Option<Vec<String>>, JsErrorBox> {
        Ok(None)
    }
}

type InkaAnalyzer = CjsModuleExportAnalyzerRc<
    InkaCjsCodeAnalyzer,
    StoreNpmChecker,
    DenoIsBuiltInNodeModuleChecker,
    StoreFolderResolver,
    RealSys,
>;

type InkaTranslator = NodeCodeTranslatorRc<
    InkaCjsCodeAnalyzer,
    StoreNpmChecker,
    DenoIsBuiltInNodeModuleChecker,
    StoreFolderResolver,
    RealSys,
>;

/// Node/CJS services the engine needs. `NodeServices::new` also returns the
/// `NodeExtInitServices` value to hand to `WorkerServiceOptions`.
pub(crate) type InkaNodeServices =
    NodeExtInitServices<StoreNpmChecker, StoreFolderResolver, RealSys>;

#[derive(Clone)]
pub(crate) struct NodeServices {
    roots: StoreRoots,
    pkg_json: PackageJsonResolverRc<RealSys>,
    analyzer: InkaAnalyzer,
    translator: InkaTranslator,
}

impl NodeServices {
    pub(crate) fn new(roots: StoreRoots) -> (Self, InkaNodeServices) {
        let sys = RealSys;
        let pkg_json: PackageJsonResolverRc<RealSys> =
            new_rc(PackageJsonResolver::new(sys.clone(), None));
        let checker = StoreNpmChecker {
            roots: roots.clone(),
        };
        let folder = StoreFolderResolver {
            roots: roots.clone(),
        };
        let node_resolver: NodeResolverRc<StoreNpmChecker, StoreFolderResolver, RealSys> =
            new_rc(NodeResolver::new(
                checker.clone(),
                DenoIsBuiltInNodeModuleChecker,
                folder.clone(),
                pkg_json.clone(),
                NodeResolutionSys::new(sys.clone(), None),
                NodeResolverOptions {
                    conditions: NodeConditionOptions::default(),
                    is_browser_platform: false,
                    bundle_mode: false,
                    typescript_version: None,
                },
            ));
        let analyzer: InkaAnalyzer = new_rc(CjsModuleExportAnalyzer::new(
            InkaCjsCodeAnalyzer {
                roots: roots.clone(),
            },
            checker,
            node_resolver.clone(),
            folder,
            pkg_json.clone(),
            sys.clone(),
        ));
        let translator: InkaTranslator = new_rc(NodeCodeTranslator::new(
            analyzer.clone(),
            NodeCodeTranslatorMode::ModuleLoader,
        ));
        let node_require_loader: NodeRequireLoaderRc = Rc::new(StoreRequireLoader {
            roots: roots.clone(),
            pkg_json: pkg_json.clone(),
        });
        let services = NodeExtInitServices {
            node_require_loader,
            node_resolver,
            pkg_json_resolver: pkg_json.clone(),
            sys,
        };
        (
            Self {
                roots,
                pkg_json,
                analyzer,
                translator,
            },
            services,
        )
    }

    /// Whether a module served to the ESM loader *might* be CommonJS. App code
    /// defaults to ESM; package roots default to CJS. The analyzer makes the
    /// final call by parsing (so an ESM file in a package root passes through).
    pub(crate) fn maybe_cjs(&self, path: &Path) -> bool {
        match extension(path).as_deref() {
            Some("cjs" | "cts") => true,
            Some("mjs" | "mts" | "json") => false,
            _ => match self
                .pkg_json
                .get_closest_package_json(path)
                .ok()
                .flatten()
                .map(|pkg| pkg.typ.clone())
            {
                Some(t) if t == "module" => false,
                Some(t) if t == "commonjs" => true,
                _ => self.roots.in_package_root(path),
            },
        }
    }

    /// If `source` is CommonJS, return the equivalent ESM facade; `None` when
    /// it is already an ES module (serve the original).
    pub(crate) async fn cjs_facade(
        &self,
        specifier: &Url,
        source: String,
    ) -> Result<Option<String>, String> {
        let resolved = self
            .analyzer
            .analyze_all_exports(specifier, Some(Cow::Borrowed(&source)), None)
            .await
            .map_err(|e| e.to_string())?;
        match resolved {
            ResolvedCjsAnalysis::Esm(_) => Ok(None),
            ResolvedCjsAnalysis::Cjs(_) => {
                let out = self
                    .translator
                    .translate_cjs_to_esm(specifier, Some(Cow::Owned(source)))
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(Some(out.into_owned()))
            }
        }
    }
}

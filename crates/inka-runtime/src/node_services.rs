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
// This module is now the single resolution policy too: ESM `import` and CJS
// `require()` both go through the store-backed `NodeResolver` here (vendored ->
// store -> builtins precedence, jsr-mirror identities, `npm:`/`jsr:` pins).

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
    DenoIsBuiltInNodeModuleChecker, InNpmPackageChecker, NodeConditionOptions, NodeResolutionKind,
    NodeResolverOptions, NpmPackageFolderResolver, PackageJsonResolver, PackageJsonResolverRc,
    ResolutionMode, UrlOrPathRef,
};
use sys_traits::impls::RealSys;

use deno_semver::{Version, VersionReq};

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

    /// The `node_modules` directory of the vendored tree
    /// (`<INKA_VENDOR>/node_modules`), if a vendor root is configured.
    fn vendor_node_modules(&self) -> Option<PathBuf> {
        self.vendor.as_ref().map(|v| v.join("node_modules"))
    }

    /// The project's own `node_modules` (`<artifact-root>/node_modules`): for
    /// `inka run` this is the project directory, for a built artifact the
    /// extracted tree. This is the bring-your-own-node_modules (BYONM) tier.
    fn project_node_modules(&self) -> Option<PathBuf> {
        self.artifact.as_ref().map(|a| a.join("node_modules"))
    }

    /// The default store's `node_modules` pool.
    fn store_node_modules(&self) -> Option<PathBuf> {
        self.store.as_ref().map(|s| s.join("node_modules"))
    }

    /// Under a `node_modules` directory of a trusted root (store/vendored/
    /// project), or anywhere in the vendored tree. Used to default a `.js`
    /// without an explicit `"type"` to CommonJS.
    fn in_package_root(&self, path: &Path) -> bool {
        [
            self.vendor_node_modules(),
            self.project_node_modules(),
            self.store_node_modules(),
        ]
        .into_iter()
        .flatten()
        .any(|nm| path.starts_with(nm))
            || self.vendor.as_ref().is_some_and(|v| path.starts_with(v))
    }
}

/// A `node_modules` root plus the tree it belongs to (the boundary the
/// nearest-`node_modules` walk may climb to).
struct NodeModulesRoot {
    nm: PathBuf,
    tree: PathBuf,
}

impl NodeModulesRoot {
    fn new(nm: PathBuf) -> Self {
        let tree = nm
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| nm.clone());
        Self { nm, tree }
    }

    /// A package's folder if `nm/<name>/package.json` exists (hoisted).
    fn hoisted(&self, name: &str) -> Option<PathBuf> {
        let candidate = self.nm.join(name);
        candidate
            .join("package.json")
            .is_file()
            .then_some(candidate)
    }

    /// Node-style nearest-`node_modules` lookup: walk up from `referrer`,
    /// checking `<dir>/node_modules/<name>`, stopping at this root's tree
    /// boundary (so nested packages beat hoisted ones).
    fn nearest(&self, referrer: &Path, name: &str) -> Option<PathBuf> {
        let mut dir = referrer.parent();
        while let Some(d) = dir {
            if !d.starts_with(&self.tree) {
                break;
            }
            let candidate = d.join("node_modules").join(name);
            if candidate.join("package.json").is_file() {
                return Some(candidate);
            }
            dir = d.parent();
        }
        None
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

/// Store/vendored identities a bare name may map to. A scoped `@scope/name`
/// also tries the jsr npm-mirror identity `@jsr/scope__name` (jsr's convention),
/// matching how the store ships `@std/assert` as `@jsr/std__assert`.
fn package_candidates(spec: &str) -> Vec<String> {
    let name = package_name(spec);
    let mut out = vec![name.clone()];
    if let Some(body) = name.strip_prefix('@') {
        if let Some((scope, pkg)) = body.split_once('/') {
            if scope != "jsr" {
                out.push(format!("@jsr/{scope}__{pkg}"));
            }
        }
    }
    out
}

/// One parsed `npm:`/`jsr:` specifier, normalized to its npm identity.
struct PkgSpec {
    /// npm package name, e.g. `zod` or `@jsr/std__assert`.
    name: String,
    /// Optional version requirement text as written (no leading `@`).
    req: Option<String>,
    /// Optional subpath (no leading `/`).
    sub: Option<String>,
}

fn parse_pkg_specifier(spec: &str) -> Result<PkgSpec, String> {
    let body = if let Some(rest) = spec.strip_prefix("npm:") {
        rest.to_string()
    } else if let Some(rest) = spec.strip_prefix("jsr:") {
        // jsr:@scope/name -> npm @jsr/scope__name (jsr's npm-compatibility mirror).
        let rest = rest.trim();
        let (scope, after) = rest.split_once('/').ok_or_else(|| {
            format!("invalid jsr specifier '{spec}' (expected jsr:@scope/name[...])")
        })?;
        let scope = scope.strip_prefix('@').unwrap_or(scope);
        let (name, tail) = split_name_suffix(after);
        format!("@jsr/{scope}__{name}{tail}")
    } else {
        return Err(format!("not a package specifier: '{spec}'"));
    };
    parse_npm_body(&body)
}

/// Splits `name` from the rest of a package body (`name[@req][/sub]`).
fn split_name_suffix(after: &str) -> (&str, &str) {
    match after.find(['@', '/']) {
        Some(i) => (&after[..i], &after[i..]),
        None => (after, ""),
    }
}

fn parse_npm_body(body: &str) -> Result<PkgSpec, String> {
    let body = body.trim();
    let (name, rest) = if body.starts_with('@') {
        let (scope, after) = body
            .split_once('/')
            .ok_or_else(|| format!("malformed scoped package '{body}'"))?;
        let (nm, rest) = split_name_suffix(after);
        (format!("{scope}/{nm}"), rest)
    } else {
        let (nm, rest) = split_name_suffix(body);
        (nm.to_string(), rest)
    };
    let mut req = None;
    let mut sub = None;
    if let Some(tail) = rest.strip_prefix('@') {
        let (r, s) = match tail.split_once('/') {
            Some((r, s)) => (r, Some(s.to_string())),
            None => (tail, None),
        };
        req = Some(r.to_string());
        sub = s;
    } else if let Some(s) = rest.strip_prefix('/') {
        sub = Some(s.to_string());
    }
    Ok(PkgSpec { name, req, sub })
}

fn version_satisfies(v: &Version, req: &str) -> bool {
    let req = req.trim();
    if let Ok(exact) = Version::parse_standard(req) {
        return v == &exact;
    }
    match VersionReq::parse_from_npm(req) {
        Ok(vr) => vr.tag().is_none() && vr.matches(v),
        Err(_) => false,
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
        let candidates = package_candidates(specifier);
        let ref_path = referrer.path().ok();

        // Resolution roots in precedence order, each with whether the referrer
        // lives inside that tree (so it resolves via the nearest-`node_modules`
        // walk) or only contributes its hoisted `node_modules/<name>`.
        //
        //   vendored  -> project node_modules (BYONM) -> default store
        //
        // A referrer inside the default store never consults vendored/project
        // trees (store packages are machine-wide and self-contained).
        let in_store =
            ref_path.is_some_and(|p| self.roots.store.as_ref().is_some_and(|s| p.starts_with(s)));
        let mut order: Vec<(PathBuf, bool)> = Vec::new();
        if in_store {
            if let Some(nm) = self.roots.store_node_modules() {
                order.push((nm, true));
            }
        } else {
            let in_vendor = ref_path
                .is_some_and(|p| self.roots.vendor.as_ref().is_some_and(|v| p.starts_with(v)));
            if let Some(nm) = self.roots.vendor_node_modules() {
                order.push((nm, in_vendor));
            }
            if let Some(nm) = self.roots.project_node_modules() {
                order.push((nm, !in_vendor && ref_path.is_some()));
            }
            if let Some(nm) = self.roots.store_node_modules() {
                order.push((nm, false));
            }
        }

        for (nm, use_nearest) in &order {
            let root = NodeModulesRoot::new(nm.clone());
            for name in &candidates {
                let found = if *use_nearest {
                    ref_path
                        .and_then(|p| root.nearest(p, name))
                        .or_else(|| root.hoisted(name))
                } else {
                    root.hoisted(name)
                };
                if let Some(f) = found {
                    return Ok(f);
                }
            }
        }

        Err(PackageFolderResolveError(Box::new(
            PackageFolderResolveErrorKind::PackageNotFound(PackageNotFoundError {
                package_name: candidates
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| package_name(specifier)),
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
    node_resolver: NodeResolverRc<StoreNpmChecker, StoreFolderResolver, RealSys>,
    folder: StoreFolderResolver,
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
            folder.clone(),
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
            node_resolver: node_resolver.clone(),
            pkg_json_resolver: pkg_json.clone(),
            sys,
        };
        (
            Self {
                roots,
                pkg_json,
                node_resolver,
                folder,
                analyzer,
                translator,
            },
            services,
        )
    }

    /// Resolve an ESM import specifier through deno's store-backed `NodeResolver`
    /// (the same policy CJS `require()` uses): bare packages, `npm:`/`jsr:` pins
    /// (normalized to their npm identity), builtins, and relative/`file:`/`data:`
    /// specifiers. Network imports are rejected.
    pub(crate) fn resolve_specifier(&self, specifier: &str, referrer: &str) -> Result<Url, String> {
        if specifier.starts_with("http://") || specifier.starts_with("https://") {
            return Err(format!(
                "network module imports are disabled ('{specifier}'); \
                 vendor the package with `inka add` instead"
            ));
        }
        if specifier.starts_with("npm:") || specifier.starts_with("jsr:") {
            let referrer_url = Url::parse(referrer).map_err(|e| e.to_string())?;
            let ps = parse_pkg_specifier(specifier)?;
            if let Some(req) = ps.req.as_deref().filter(|s| !s.trim().is_empty()) {
                self.check_pin(&ps.name, req, &referrer_url)?;
            }
            let spec = match &ps.sub {
                Some(sub) => format!("{}/{}", ps.name, sub),
                None => ps.name.clone(),
            };
            return self.resolve_with_node(&spec, &referrer_url);
        }
        // Schemes (`node:`/`file:`/`data:`/…) and relative/absolute specifiers.
        // The referrer can be a non-URL (deno passes "." for the root module);
        // `resolve_import` only needs a base for relative specifiers.
        if specifier.contains(':')
            || specifier.starts_with("./")
            || specifier.starts_with("../")
            || specifier.starts_with('/')
        {
            return deno_core::resolve_import(specifier, referrer).map_err(|e| e.to_string());
        }
        let referrer_url = Url::parse(referrer).map_err(|e| e.to_string())?;
        self.resolve_with_node(specifier, &referrer_url)
    }

    fn resolve_with_node(&self, spec: &str, referrer: &Url) -> Result<Url, String> {
        self.node_resolver
            .resolve(
                spec,
                referrer,
                ResolutionMode::Import,
                NodeResolutionKind::Execution,
            )
            .map_err(|e| e.to_string())?
            .into_url()
            .map_err(|e| e.to_string())
    }

    /// Enforce an `npm:`/`jsr:` version pin against the installed (vendored or
    /// store) package, mirroring the retired resolver's check.
    fn check_pin(&self, name: &str, req: &str, referrer: &Url) -> Result<(), String> {
        let referrer_ref = UrlOrPathRef::from_url(referrer);
        let Ok(pkg_root) = self
            .folder
            .resolve_package_folder_from_package(name, &referrer_ref)
        else {
            return Ok(()); // let NodeResolver surface the not-found error
        };
        if let Some(installed) = self.installed_version(&pkg_root) {
            if !version_satisfies(&installed, req) {
                return Err(format!(
                    "package '{name}' is installed at {installed}, which does not satisfy \
                     '{req}'; run `inka update` to install the requested version"
                ));
            }
        }
        Ok(())
    }

    fn installed_version(&self, pkg_root: &Path) -> Option<Version> {
        let pkg = self
            .pkg_json
            .load_package_json(&pkg_root.join("package.json"))
            .ok()
            .flatten()?;
        Version::parse_standard(pkg.version.as_deref()?).ok()
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

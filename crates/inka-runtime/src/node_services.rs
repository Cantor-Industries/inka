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
// This module is the single resolution policy too: ESM `import` and CJS
// `require()` both go through the `NodeResolver` here, rooted at the execution
// tree's `node_modules` (jsr-mirror identities, `npm:`/`jsr:` pins, builtins).

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
    NodeResolveError, PackageFolderResolveError, PackageFolderResolveErrorKind,
    PackageJsonLoadError, PackageNotFoundError,
};
use node_resolver::{
    DenoIsBuiltInNodeModuleChecker, InNpmPackageChecker, NodeConditionOptions, NodeResolutionKind,
    NodeResolverOptions, NpmPackageFolderResolver, PackageJsonResolver, PackageJsonResolverRc,
    ResolutionMode, UrlOrPath, UrlOrPathRef,
};
use sys_traits::impls::RealSys;

use deno_semver::{Version, VersionReq};

/// Filesystem root a `require()` may reach without an explicit read grant: the
/// execution tree (the project directory for `inka run`, the extracted tree for
/// a built artifact). Reads outside it stay deny-by-default.
#[derive(Clone, Default)]
pub(crate) struct ExecutionRoots {
    pub root: Option<PathBuf>,
}

impl ExecutionRoots {
    fn contains(&self, path: &Path) -> bool {
        let Some(root) = self.root.as_ref() else {
            return false;
        };
        // Compare real paths: a lexical prefix check is defeated by a symlink
        // inside the tree pointing outside it. Fall back to the lexical check
        // only when the path does not exist (nothing to resolve yet).
        match std::fs::canonicalize(path) {
            Ok(real) => real.starts_with(root),
            Err(_) => path.starts_with(root),
        }
    }

    /// The execution tree's `node_modules` (bring-your-own-node_modules).
    fn node_modules(&self) -> Option<PathBuf> {
        self.root.as_ref().map(|r| r.join("node_modules"))
    }

    /// Under the execution tree's `node_modules`. Used to default a `.js`
    /// without an explicit `"type"` to CommonJS.
    fn in_package_root(&self, path: &Path) -> bool {
        self.node_modules().is_some_and(|nm| path.starts_with(nm))
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

/// Package identities a bare name may map to. A scoped `@scope/name` also
/// tries the jsr npm-mirror identity `@jsr/scope__name` (jsr's convention,
/// e.g. `@std/assert` as `@jsr/std__assert`).
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

/// Validate an npm package identity: `name` or `@scope/name`. Rejects empty,
/// traversal (`.`/`..`), separators, and absolute paths so a name can never
/// escape the `node_modules` it is joined to.
fn valid_package_name(name: &str) -> bool {
    if name.is_empty()
        || name.contains('\\')
        || name.contains('\0')
        || Path::new(name).is_absolute()
    {
        return false;
    }
    let component_ok = |s: &str| !s.is_empty() && s != "." && s != "..";
    if let Some(rest) = name.strip_prefix('@') {
        match rest.split_once('/') {
            Some((scope, pkg)) => component_ok(scope) && component_ok(pkg) && !pkg.contains('/'),
            None => false,
        }
    } else {
        component_ok(name) && !name.contains('/')
    }
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
    if !valid_package_name(&name) {
        return Err(format!("invalid package name '{name}'"));
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
pub(crate) struct ExecutionNpmChecker {
    roots: ExecutionRoots,
}

impl InNpmPackageChecker for ExecutionNpmChecker {
    fn in_npm_package(&self, specifier: &Url) -> bool {
        specifier
            .to_file_path()
            .map(|p| self.roots.contains(&p))
            .unwrap_or(false)
    }
}

#[derive(Clone)]
pub(crate) struct ExecutionFolderResolver {
    roots: ExecutionRoots,
}

impl NpmPackageFolderResolver for ExecutionFolderResolver {
    fn resolve_package_folder_from_package(
        &self,
        specifier: &str,
        referrer: &UrlOrPathRef,
    ) -> Result<PathBuf, PackageFolderResolveError> {
        let candidates = package_candidates(specifier);
        // Node/Deno resolve the referrer's realpath before walking
        // `node_modules`. This is required for symlinked stores (pnpm
        // `.pnpm/`, yarn, bun), where a package's dependencies live beside its
        // realpath rather than at the project root.
        let ref_path = referrer
            .path()
            .ok()
            .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()));

        // A single root: the execution tree's `node_modules`. A referrer inside
        // the tree resolves via the nearest-`node_modules` walk (nested beats
        // hoisted); a referrer outside it only reaches the hoisted
        // `node_modules/<name>`.
        if let Some(nm) = self.roots.node_modules() {
            let root = NodeModulesRoot::new(nm);
            for name in &candidates {
                // Never join a name that could traverse out of `node_modules`.
                if !valid_package_name(name) {
                    continue;
                }
                let found = match ref_path.as_deref() {
                    Some(p) => root.nearest(p, name).or_else(|| root.hoisted(name)),
                    None => root.hoisted(name),
                };
                if let Some(f) = found {
                    // Return the realpath so the package's own deps resolve from
                    // its real location too. A symlink whose realpath leaves the
                    // execution tree stays gated by `ExecutionRoots::contains`
                    // (canonical) on the require read path.
                    return Ok(std::fs::canonicalize(&f).unwrap_or(f));
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

struct ExecutionRequireLoader {
    roots: ExecutionRoots,
    pkg_json: PackageJsonResolverRc<RealSys>,
}

impl NodeRequireLoader for ExecutionRequireLoader {
    fn ensure_read_permission<'a>(
        &self,
        permissions: &mut PermissionsContainer,
        path: Cow<'a, Path>,
    ) -> Result<Cow<'a, Path>, JsErrorBox> {
        // Reads inside the execution tree are implicit (the ESM loader already
        // confines module reads). Anything else is deny-by-default unless
        // `--allow-read` grants it (Deno semantics).
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
    roots: ExecutionRoots,
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
                "refusing to analyze a CJS module outside the execution tree: {specifier}"
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
    ExecutionNpmChecker,
    DenoIsBuiltInNodeModuleChecker,
    ExecutionFolderResolver,
    RealSys,
>;

type InkaTranslator = NodeCodeTranslatorRc<
    InkaCjsCodeAnalyzer,
    ExecutionNpmChecker,
    DenoIsBuiltInNodeModuleChecker,
    ExecutionFolderResolver,
    RealSys,
>;

/// Node/CJS services the engine needs. `NodeServices::new` also returns the
/// `NodeExtInitServices` value to hand to `WorkerServiceOptions`.
pub(crate) type InkaNodeServices =
    NodeExtInitServices<ExecutionNpmChecker, ExecutionFolderResolver, RealSys>;

#[derive(Clone)]
pub(crate) struct NodeServices {
    roots: ExecutionRoots,
    pkg_json: PackageJsonResolverRc<RealSys>,
    node_resolver: NodeResolverRc<ExecutionNpmChecker, ExecutionFolderResolver, RealSys>,
    folder: ExecutionFolderResolver,
    analyzer: InkaAnalyzer,
    translator: InkaTranslator,
}

impl NodeServices {
    pub(crate) fn new(roots: ExecutionRoots) -> (Self, InkaNodeServices) {
        let sys = RealSys;
        let pkg_json: PackageJsonResolverRc<RealSys> =
            new_rc(PackageJsonResolver::new(sys.clone(), None));
        let checker = ExecutionNpmChecker {
            roots: roots.clone(),
        };
        let folder = ExecutionFolderResolver {
            roots: roots.clone(),
        };
        let node_resolver: NodeResolverRc<ExecutionNpmChecker, ExecutionFolderResolver, RealSys> =
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
        let node_require_loader: NodeRequireLoaderRc = Rc::new(ExecutionRequireLoader {
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

    /// Resolve an ESM import specifier through deno's `NodeResolver` (the same
    /// policy CJS `require()` uses): bare packages, `npm:`/`jsr:` pins
    /// (normalized to their npm identity), builtins, and relative/`file:`/`data:`
    /// specifiers. Network imports are rejected.
    pub(crate) fn resolve_specifier(&self, specifier: &str, referrer: &str) -> Result<Url, String> {
        if specifier.starts_with("http://") || specifier.starts_with("https://") {
            return Err(format!(
                "network module imports are disabled ('{specifier}')"
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
        match self.node_resolver.resolve(
            spec,
            referrer,
            ResolutionMode::Import,
            NodeResolutionKind::Execution,
        ) {
            Ok(res) => res.into_url().map_err(|e| e.to_string()),
            Err(err) => {
                if let Some(url) = self.recover_ts_specifier(&err) {
                    Ok(url)
                } else {
                    Err(err.to_string())
                }
            }
        }
    }

    /// `node_resolver` treats a `.js`/`.mjs`/`.cjs` specifier literally and
    /// raises `ModuleNotFound` when only the `.ts` source exists (the TS
    /// "write .js, ship .ts" convention). Recover by rewriting the failed
    /// specifier to its TypeScript sibling when that file exists in-tree.
    fn recover_ts_specifier(&self, err: &NodeResolveError) -> Option<Url> {
        let spec = err.maybe_specifier()?;
        let path = match spec.as_ref() {
            UrlOrPath::Path(p) => p.to_path_buf(),
            UrlOrPath::Url(u) => u.to_file_path().ok()?,
        };
        for cand in crate::ts_rewrite_candidates(&path) {
            if !cand.is_file() {
                continue;
            }
            let real = std::fs::canonicalize(&cand).ok()?;
            if !self.roots.contains(&real) {
                continue;
            }
            return Url::from_file_path(&real).ok();
        }
        None
    }

    /// Enforce an `npm:`/`jsr:` version pin against the installed package.
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
                     '{req}'"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_name_validation() {
        for ok in ["ms", "@scope/name", "a-b.c", "@jsr/std__assert"] {
            assert!(valid_package_name(ok), "{ok} should be valid");
        }
        for bad in [
            "", ".", "..", "a/b", "a\\b", "/abs", "@scope", "@scope/", "@/x", "@a/b/c", "@./x",
            "@a/..",
        ] {
            assert!(!valid_package_name(bad), "{bad} should be invalid");
        }
    }

    #[cfg(unix)]
    #[test]
    fn contains_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let base = std::env::temp_dir().join(format!("inka-rt-contain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.js"), b"x").unwrap();
        std::fs::write(root.join("real.js"), b"x").unwrap();
        symlink(outside.join("secret.js"), root.join("link.js")).unwrap();

        let roots = ExecutionRoots {
            root: Some(std::fs::canonicalize(&root).unwrap()),
        };
        assert!(
            roots.contains(&root.join("real.js")),
            "in-tree file allowed"
        );
        assert!(
            !roots.contains(&root.join("link.js")),
            "symlink escaping the tree must be rejected"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}

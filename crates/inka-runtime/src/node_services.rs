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
use deno_runtime::deno_permissions::PermissionsContainer;
use node_resolver::cache::NodeResolutionSys;
use node_resolver::errors::{
    PackageFolderResolveError, PackageFolderResolveErrorKind, PackageJsonLoadError,
    PackageNotFoundError,
};
use node_resolver::{
    DenoIsBuiltInNodeModuleChecker, InNpmPackageChecker, NpmPackageFolderResolver,
    NodeConditionOptions, NodeResolverOptions, PackageJsonResolver, PackageJsonResolverRc,
    UrlOrPathRef,
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

impl StoreRequireLoader {
    /// `.cjs`/`.cts` are CJS; `.mjs`/`.mts`/`.json` are not; other extensions
    /// follow the nearest `package.json` `"type"` (absent/non-module => CJS).
    fn is_cjs(&self, specifier: &Url) -> bool {
        let Ok(path) = specifier.to_file_path() else {
            return false;
        };
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        match ext.as_deref() {
            Some("cjs" | "cts") => true,
            Some("mjs" | "mts" | "json") => false,
            _ => match self.pkg_json.get_closest_package_json(&path) {
                Ok(Some(pkg)) => pkg.typ != "module",
                _ => true,
            },
        }
    }
}

impl NodeRequireLoader for StoreRequireLoader {
    fn ensure_read_permission<'a>(
        &self,
        _permissions: &mut PermissionsContainer,
        path: Cow<'a, Path>,
    ) -> Result<Cow<'a, Path>, JsErrorBox> {
        if self.roots.contains(path.as_ref()) {
            return Ok(path);
        }
        // Outside the trusted roots: deny-by-default. Permission-aware reads
        // are a later refinement; the ESM loader already confines module reads.
        Err(JsErrorBox::generic(format!(
            "require read outside the package store/vendored tree is not allowed: {}",
            path.display()
        )))
    }

    fn load_text_file_lossy(&self, path: &Path) -> Result<FastString, JsErrorBox> {
        let bytes = std::fs::read(path).map_err(JsErrorBox::from_err)?;
        Ok(FastString::from(String::from_utf8_lossy(&bytes).into_owned()))
    }

    fn is_maybe_cjs(&self, specifier: &Url) -> Result<bool, PackageJsonLoadError> {
        Ok(self.is_cjs(specifier))
    }

    fn is_maybe_cjs_from_require(&self, specifier: &Url) -> Result<bool, PackageJsonLoadError> {
        Ok(self.is_cjs(specifier))
    }
}

/// Build the node services deno_node needs, backed by the inka store/vendored
/// roots. This is the only constructor the engine calls.
pub(crate) fn build_node_services(
    roots: StoreRoots,
) -> NodeExtInitServices<StoreNpmChecker, StoreFolderResolver, RealSys> {
    let sys = RealSys;
    let pkg_json: PackageJsonResolverRc<RealSys> =
        new_rc(PackageJsonResolver::new(sys.clone(), None));
    let node_resolver: NodeResolverRc<StoreNpmChecker, StoreFolderResolver, RealSys> = new_rc(
        NodeResolver::new(
            StoreNpmChecker {
                roots: roots.clone(),
            },
            DenoIsBuiltInNodeModuleChecker,
            StoreFolderResolver {
                roots: roots.clone(),
            },
            pkg_json.clone(),
            NodeResolutionSys::new(sys.clone(), None),
            NodeResolverOptions {
                conditions: NodeConditionOptions::default(),
                is_browser_platform: false,
                bundle_mode: false,
                typescript_version: None,
            },
        ),
    );
    let node_require_loader: NodeRequireLoaderRc = Rc::new(StoreRequireLoader {
        roots,
        pkg_json: pkg_json.clone(),
    });
    NodeExtInitServices {
        node_require_loader,
        node_resolver,
        pkg_json_resolver: pkg_json,
        sys,
    }
}

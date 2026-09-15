//! inka bundler: resolve a JS/TS entry (import maps + `npm:`/`jsr:` from the
//! Deno cache and `node_modules`) and emit a single self-contained ESM bundle.

mod resolve;

use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;

use deno_semver::npm::NpmPackageReqReference;
use deno_semver::Version;
use rolldown::plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookResolveIdArgs, HookResolveIdOutput,
    HookResolveIdReturn, HookUsage, Plugin, PluginContext, Pluginable, SharedLoadPluginContext,
};
use rolldown::{
    BundlerBuilder, BundlerOptions, CodeSplittingMode, InputItem, IsExternal, OutputFormat,
    Platform, RawMinifyOptions, SourceMapType, TreeshakeOptions,
};
use rolldown_common::{ModuleType, Output};
use rolldown_utils::indexmap::FxIndexMap;
use sys_traits::impls::RealSys;
use url::Url;

use resolve::ResolverState;

/// Options for a single bundle.
pub struct BundleOptions<'a> {
    /// Project root (also the rolldown `cwd`).
    pub cwd: &'a Path,
    /// Entry file, relative to `cwd`.
    pub entry: &'a str,
    /// Package specifiers to leave external (embedded as files by the caller).
    pub external: &'a [String],
    pub minify: bool,
    pub sourcemap: bool,
}

/// Package names this bundler keeps external by default (without a CLI
/// `--external`). They are embedded as packages rather than inlined because
/// they resolve companion files relative to their own install path at run
/// time; inlining breaks that. TypeScript is the canonical case: `ts.sys`
/// finds `lib.*.d.ts` next to `typescript.js`, so it must travel as a package.
const DEFAULT_EXTERNAL: &[&str] = &["typescript"];

/// External matcher: a specifier is external when it equals a package name or
/// is a subpath of one (`pkg` or `pkg/sub`), so `--external typescript` also
/// covers `typescript/lib/tsserverlibrary`.
fn external_matcher(packages: Vec<String>) -> IsExternal {
    let f = move |spec: &str, _importer: Option<&str>, _is_resolved: bool| {
        let packages = packages.clone();
        let spec = spec.to_string();
        let fut: std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<bool>> + Send>> =
            Box::pin(async move {
                Ok(packages
                    .iter()
                    .any(|p| spec == *p || spec.starts_with(&format!("{p}/"))))
            });
        fut
    };
    IsExternal::Fn(Some(Arc::new(f)))
}

/// A produced bundle plus any files the caller should embed alongside it.
pub struct Bundle {
    pub code: String,
    pub embedded: Vec<(String, Vec<u8>)>,
    pub warnings: Vec<String>,
    /// Default-external packages the emitted chunk actually imports. The caller
    /// embeds these (as `--external` packages) so the artifact stays
    /// self-contained. Empty when the chunk does not reference them.
    pub auto_embed: Vec<String>,
}

/// Bundle `entry` into a single ESM module. Owns its own tokio runtime.
pub fn bundle(opts: BundleOptions<'_>) -> Result<Bundle, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to build tokio runtime: {e}"))?;
    rt.block_on(bundle_async(opts))
}

async fn bundle_async(opts: BundleOptions<'_>) -> Result<Bundle, String> {
    let entry_path = opts.cwd.join(opts.entry);
    let state = resolve::build(opts.cwd, &entry_path).await;

    // Best-effort native/asset candidates: any resolved `.node` file.
    let mut embedded: Vec<(String, Vec<u8>)> = Vec::new();
    for resolved in state.edges.values() {
        if resolved.ends_with(".node") {
            if let Some(path) = Url::parse(resolved)
                .ok()
                .and_then(|u| u.to_file_path().ok())
            {
                if let (Ok(bytes), Ok(rel)) = (std::fs::read(&path), path.strip_prefix(opts.cwd)) {
                    embedded.push((rel.to_string_lossy().replace('\\', "/"), bytes));
                }
            }
        }
    }

    let plugin = Arc::new(DenoResolvePlugin { state });
    let shared: Arc<dyn Pluginable> = plugin.clone();

    // Bundled CJS modules reference the Node ambient `__filename`/`__dirname`,
    // but the emitted chunk is ESM, where those are undefined (rolldown's
    // `__commonJS` wrapper supplies only `exports`/`module`). Map them to the
    // Node/Deno ESM equivalents; the shim is applied to every bundled module and
    // only rewrites global (non-shadowed) references.
    let mut define = FxIndexMap::default();
    define.insert("__filename".to_string(), "import.meta.filename".to_string());
    define.insert("__dirname".to_string(), "import.meta.dirname".to_string());

    // User `--external` plus the packages we keep external by default (so they
    // are embedded as packages, not inlined). Only those actually imported by
    // the emitted chunk are reported back in `auto_embed`.
    let mut external_list: Vec<String> = opts.external.to_vec();
    for pkg in DEFAULT_EXTERNAL {
        if !external_list.iter().any(|e| e == pkg) {
            external_list.push((*pkg).to_string());
        }
    }

    let options = BundlerOptions {
        input: Some(vec![InputItem {
            name: Some("chunk".to_string()),
            import: opts.entry.to_string(),
        }]),
        cwd: Some(opts.cwd.to_path_buf()),
        platform: Some(Platform::Node),
        format: Some(OutputFormat::Esm),
        external: Some(external_matcher(external_list)),
        treeshake: TreeshakeOptions::default(),
        // A single self-contained chunk: dynamic imports are inlined rather
        // than split into sibling files the artifact would not carry.
        code_splitting: Some(CodeSplittingMode::Bool(false)),
        minify: opts.minify.then_some(RawMinifyOptions::Bool(true)),
        // Inline (data-URL) maps keep the artifact self-contained: the caller
        // only carries `chunk.code`, so a `File` map would be silently dropped.
        sourcemap: opts.sourcemap.then_some(SourceMapType::Inline),
        define: Some(define),
        // Execute modules in the order they are declared. Scope hoisting turns
        // `class X extends Y` into `var X = class extends Y`, and when rolldown
        // orders a dependent before its dependency `Y` is hoisted-but-unset
        // (→ "Class extends value undefined"). `strict_execution_order` emits
        // execution-order helpers so dependencies initialize first;
        // `on_demand_wrapping` restricts that wrapping to modules that actually
        // need it (the blanket variant mis-orders valid re-export/barrel
        // cycles). Correctness beats the bundle-size cost for an app artifact.
        strict_execution_order: Some(true),
        experimental: Some(rolldown::ExperimentalOptions {
            on_demand_wrapping: Some(true),
            ..Default::default()
        }),
        // Elide imports that are only used in type positions even when the
        // project sets tsconfig `verbatimModuleSyntax: true`. Keeping such
        // imports introduces runtime cycles (`a.ts` imports a type from `b.ts`
        // while `b.ts` imports a value from `a.ts`) that break `class extends`
        // at module init. This matches the runtime (Bun/Deno) and the common
        // bundler default. Side-effect-only imports (`import "./x"`) are still
        // preserved; only unused bindings are dropped.
        transform: Some(rolldown::BundlerTransformOptions {
            typescript: Some(rolldown::TypeScriptOptions {
                only_remove_type_imports: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };

    let mut bundler = BundlerBuilder::default()
        .with_options(options)
        .with_plugins(vec![shared])
        .build()
        .map_err(|e| format!("failed to construct rolldown bundler: {e}"))?;
    let output = bundler
        .generate()
        .await
        .map_err(|e| format!("rolldown generate failed: {e}"))?;

    let mut chunks = Vec::new();
    // External specifiers the emitted chunk imports (static + dynamic). Used to
    // decide which default-external packages the caller must embed.
    let mut external_imports: Vec<String> = Vec::new();
    for asset in &output.assets {
        if let Output::Chunk(chunk) = asset {
            chunks.push(chunk.code.clone());
            for spec in chunk.imports.iter().chain(chunk.dynamic_imports.iter()) {
                external_imports.push(spec.to_string());
            }
        }
    }
    if chunks.len() != 1 {
        return Err(format!(
            "rolldown produced {} chunks; expected a single bundle",
            chunks.len()
        ));
    }
    let code = chunks.into_iter().next().unwrap();

    // Report default-external packages the chunk imports (including subpath
    // imports like `typescript/lib/tsserverlibrary`) so the caller embeds them.
    let mut auto_embed: Vec<String> = Vec::new();
    for pkg in DEFAULT_EXTERNAL {
        let imported = external_imports
            .iter()
            .any(|spec| spec == pkg || spec.starts_with(&format!("{pkg}/")));
        if imported && !auto_embed.iter().any(|p| p == pkg) {
            auto_embed.push((*pkg).to_string());
        }
    }

    // Surface rolldown's warnings instead of dropping them, so things it flags
    // (notably direct `eval`, which a scope-hoisting bundle cannot preserve
    // correctly) reach the user at build time. The `verbatimModuleSyntax`
    // override is intentional (see `transform` above), so it is not surfaced.
    let warnings = output
        .warnings
        .iter()
        .map(ToString::to_string)
        .filter(|w| !w.contains("onlyRemoveTypeImports"))
        .collect();

    Ok(Bundle {
        code,
        embedded,
        warnings,
        auto_embed,
    })
}

/// rolldown plugin that resolves import maps, `npm:`, `jsr:`, and serves cached
/// jsr sources.
struct DenoResolvePlugin {
    state: ResolverState,
}

impl std::fmt::Debug for DenoResolvePlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DenoResolvePlugin")
    }
}

impl Plugin for DenoResolvePlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("inka-deno-resolve")
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ResolveId | HookUsage::Load
    }

    async fn resolve_id(
        &self,
        ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        let spec = args.specifier;
        let importer = args.importer;

        // Relative import from a remote (jsr) module: join against the URL.
        if let Some(imp) = importer {
            if (imp.starts_with("https://") || imp.starts_with("http://"))
                && (spec.starts_with("./") || spec.starts_with("../"))
            {
                if let Ok(base) = Url::parse(imp) {
                    if let Ok(joined) = base.join(spec) {
                        return Ok(Some(HookResolveIdOutput::from_id(joined.to_string())));
                    }
                }
            }
        }

        // Direct `jsr:` specifiers (including transitive ones).
        if let Some(https) = self.state.jsr_redirects.get(spec) {
            return Ok(Some(HookResolveIdOutput::from_id(https.clone())));
        }

        let bare = !spec.contains(':')
            && !spec.starts_with("./")
            && !spec.starts_with("../")
            && !spec.starts_with('/');

        // Apply the deno.json import map, then classify the target.
        let target = if bare {
            self.state
                .map_specifier(spec, importer)
                .unwrap_or_else(|| spec.to_string())
        } else {
            spec.to_string()
        };

        if let Some(https) = self.state.jsr_redirects.get(&target) {
            return Ok(Some(HookResolveIdOutput::from_id(https.clone())));
        }
        if target.starts_with("http://") || target.starts_with("https://") {
            return Ok(Some(HookResolveIdOutput::from_id(target)));
        }
        let npm = if let Some(rest) = target.strip_prefix("npm:") {
            npm_bare_and_req(rest)
        } else if bare {
            Some((target.clone(), None))
        } else {
            None
        };
        if let Some((spec_bare, req)) = npm {
            if let Ok(Ok(resolved)) = ctx.resolve(&spec_bare, importer, None).await {
                if let Some(req_ref) = &req {
                    check_npm_pin(&resolved, req_ref)?;
                }
                return Ok(Some(HookResolveIdOutput::from_resolved_id(resolved)));
            }
        }
        Ok(None)
    }

    async fn load(&self, _ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
        let id = args.id;
        if id.starts_with("https://") || id.starts_with("http://") {
            if let Ok(url) = Url::parse(id) {
                if let Some((code, module_type)) = read_remote_source(&self.state.deno_dir, &url) {
                    return Ok(Some(HookLoadOutput {
                        code: code.into(),
                        module_type: Some(module_type),
                        ..Default::default()
                    }));
                }
            }
        }
        Ok(None)
    }
}

/// Read a remote module from the Deno cache and infer its module type.
fn read_remote_source(deno_dir: &Path, url: &Url) -> Option<(String, ModuleType)> {
    use deno_cache_dir::{GlobalHttpCache, HttpCache};

    let cache = GlobalHttpCache::new(RealSys, deno_dir.join("remote"));
    let key = cache.cache_item_key(url).ok()?;
    let entry = cache.get(&key, None).ok()??;
    let bytes = entry.content.into_owned();
    let ext = url
        .path()
        .rsplit('.')
        .next()
        .map(|s| s.to_ascii_lowercase());
    let module_type = match ext.as_deref() {
        Some("ts" | "mts" | "cts") => ModuleType::Ts,
        Some("tsx") => ModuleType::Tsx,
        Some("jsx") => ModuleType::Jsx,
        Some("json") => ModuleType::Json,
        _ => ModuleType::Js,
    };
    Some((String::from_utf8_lossy(&bytes).into_owned(), module_type))
}

/// Parse an `npm:` body into the bare specifier rolldown resolves plus the
/// version requirement to enforce: `foo@1/sub` -> (`foo/sub`, req `1`),
/// `@scope/foo@~1.2` -> (`@scope/foo`, req `~1.2`). Uses `deno_semver`'s
/// `NpmPackageReqReference`, the same parser Deno uses.
fn npm_bare_and_req(body: &str) -> Option<(String, Option<NpmPackageReqReference>)> {
    let req_ref = NpmPackageReqReference::from_str(&format!("npm:{body}")).ok()?;
    let req = req_ref.req();
    let bare = match req_ref.sub_path() {
        Some(sub) => format!("{}/{}", req.name, sub),
        None => req.name.to_string(),
    };
    Some((bare, Some(req_ref)))
}

/// Enforce an `npm:` version requirement against the version rolldown resolved
/// (the same policy `inka run` applies when loading an `npm:` specifier).
fn check_npm_pin(
    resolved: &rolldown_common::ResolvedId,
    req_ref: &NpmPackageReqReference,
) -> anyhow::Result<()> {
    let req = req_ref.req();
    if req.version_req.version_text() == "*" {
        return Ok(());
    }
    let installed = resolved
        .package_json
        .as_deref()
        .and_then(|p| p.version())
        .map(str::to_string)
        .or_else(|| installed_version_from_id(resolved.id.as_str()));
    let Some(installed) = installed.and_then(|v| Version::parse_standard(&v).ok()) else {
        // The installed version is unknown (e.g. a workspace file dependency);
        // leave enforcement to the runtime rather than guess here.
        return Ok(());
    };
    if !req.version_req.matches(&installed) {
        return Err(anyhow::anyhow!(
            "package '{}' is installed at {}, which does not satisfy '{}'",
            req.name,
            installed,
            req.version_req
        ));
    }
    Ok(())
}

/// The `version` of the nearest `package.json` above a resolved module id.
fn installed_version_from_id(id: &str) -> Option<String> {
    let path = Url::parse(id)
        .ok()
        .and_then(|u| u.to_file_path().ok())
        .unwrap_or_else(|| std::path::PathBuf::from(id));
    let mut dir = path.parent();
    while let Some(d) = dir {
        let pkg = d.join("package.json");
        if let Ok(text) = std::fs::read_to_string(&pkg) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(ver) = v.get("version").and_then(|x| x.as_str()) {
                    return Some(ver.to_string());
                }
            }
        }
        dir = d.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn scratch() -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("inkabundle-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn mk(cwd: &Path, rel: &str, body: &str) {
        let p = cwd.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn bundles_relative_cjs_deterministically() {
        let cwd = scratch();
        mk(
            &cwd,
            "node_modules/cjs-pkg/package.json",
            r#"{"name":"cjs-pkg","version":"1.0.0","main":"index.js"}"#,
        );
        mk(
            &cwd,
            "node_modules/cjs-pkg/index.js",
            "module.exports = { add: (a, b) => a + b };\n",
        );
        mk(
            &cwd,
            "entry.js",
            "import cjs from \"cjs-pkg\";\nconsole.log(\"sum\", cjs.add(1, 2));\n",
        );

        let opts = || BundleOptions {
            cwd: &cwd,
            entry: "entry.js",
            external: &[],
            minify: false,
            sourcemap: false,
        };
        let a = bundle(opts()).unwrap();
        let b = bundle(opts()).unwrap();
        assert_eq!(a.code, b.code, "bundle must be deterministic");
        assert!(
            a.code.contains("a + b") || a.code.contains("a+b"),
            "expected inlined cjs body:\n{}",
            a.code
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn bundles_jsr_import_map_offline() {
        let deno_dir = resolve::deno_dir();
        if !deno_dir.join("remote/https/jsr.io").is_dir() {
            eprintln!("skip: no Deno remote cache at {}", deno_dir.display());
            return;
        }
        let cwd = scratch();
        mk(
            &cwd,
            "deno.json",
            r#"{"imports":{"@std/assert":"jsr:@std/assert@1"}}"#,
        );
        mk(
            &cwd,
            "entry.ts",
            "import { assertEquals } from \"@std/assert\";\nassertEquals(1, 1);\nconsole.log(\"ok\");\n",
        );
        let opts = BundleOptions {
            cwd: &cwd,
            entry: "entry.ts",
            external: &[],
            minify: false,
            sourcemap: false,
        };
        let b = bundle(opts).expect("jsr bundle must succeed when the cache is present");
        assert!(b.code.contains("assert"), "expected inlined assert code");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn external_package_is_not_inlined() {
        let cwd = scratch();
        mk(
            &cwd,
            "node_modules/ms/package.json",
            r#"{"name":"ms","version":"2.1.3","main":"index.js"}"#,
        );
        mk(
            &cwd,
            "node_modules/ms/index.js",
            "module.exports = function ms() { return \"marker-inlined\"; };\n",
        );
        mk(
            &cwd,
            "entry.js",
            "import ms from \"ms\";\nconsole.log(ms());\n",
        );
        let external = vec!["ms".to_string()];
        let opts = || BundleOptions {
            cwd: &cwd,
            entry: "entry.js",
            external: &external,
            minify: false,
            sourcemap: false,
        };
        let a = bundle(opts()).unwrap();
        let b = bundle(opts()).unwrap();
        assert_eq!(a.code, b.code, "bundle must be deterministic");
        assert!(
            !a.code.contains("marker-inlined"),
            "external package must not be inlined:\n{}",
            a.code
        );
        assert!(
            a.code.contains("ms"),
            "expected an external import of ms:\n{}",
            a.code
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn minify_is_deterministic() {
        let cwd = scratch();
        mk(
            &cwd,
            "node_modules/pkg/package.json",
            r#"{"name":"pkg","version":"1.0.0","main":"index.js"}"#,
        );
        mk(
            &cwd,
            "node_modules/pkg/index.js",
            "module.exports = { f: (a, b) => a * b };\n",
        );
        mk(
            &cwd,
            "entry.js",
            "import p from \"pkg\";\nconsole.log(p.f(6, 7));\n",
        );
        let opts = || BundleOptions {
            cwd: &cwd,
            entry: "entry.js",
            external: &[],
            minify: true,
            sourcemap: false,
        };
        let a = bundle(opts()).unwrap();
        let b = bundle(opts()).unwrap();
        assert_eq!(a.code, b.code, "minified bundle must be deterministic");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn dynamic_import_is_inlined_single_chunk() {
        let cwd = scratch();
        mk(&cwd, "lazy.js", "export const v = \"lazy-marker\";\n");
        mk(
            &cwd,
            "entry.js",
            "const m = await import(\"./lazy.js\");\nconsole.log(m.v);\n",
        );
        let opts = BundleOptions {
            cwd: &cwd,
            entry: "entry.js",
            external: &[],
            minify: false,
            sourcemap: false,
        };
        let b = bundle(opts).unwrap();
        assert!(
            b.code.contains("lazy-marker"),
            "dynamic import not inlined:\n{}",
            b.code
        );
        assert!(
            !b.code.contains("./lazy"),
            "expected no sibling chunk import:\n{}",
            b.code
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn sourcemap_is_inline() {
        let cwd = scratch();
        mk(&cwd, "entry.js", "console.log(\"map-me\");\n");
        let opts = BundleOptions {
            cwd: &cwd,
            entry: "entry.js",
            external: &[],
            minify: false,
            sourcemap: true,
        };
        let b = bundle(opts).unwrap();
        assert!(
            b.code.contains("sourceMappingURL=data:"),
            "expected an inline data-URL source map:\n{}",
            b.code
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn jsonc_import_map_is_parsed() {
        let cwd = scratch();
        mk(&cwd, "util.js", "export const u = \"jsonc-ok\";\n");
        mk(
            &cwd,
            "deno.jsonc",
            "{\n  // a comment and a trailing comma\n  \"imports\": { \"@util\": \"./util.js\", },\n}\n",
        );
        mk(
            &cwd,
            "entry.js",
            "import { u } from \"@util\";\nconsole.log(u);\n",
        );
        let opts = BundleOptions {
            cwd: &cwd,
            entry: "entry.js",
            external: &[],
            minify: false,
            sourcemap: false,
        };
        let b = bundle(opts).unwrap();
        assert!(
            b.code.contains("jsonc-ok"),
            "jsonc import map not applied:\n{}",
            b.code
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn npm_version_pin_is_enforced() {
        let cwd = scratch();
        mk(
            &cwd,
            "node_modules/foo/package.json",
            r#"{"name":"foo","version":"1.0.0","main":"index.js"}"#,
        );
        mk(
            &cwd,
            "node_modules/foo/index.js",
            "module.exports = { v: 1 };\n",
        );
        mk(
            &cwd,
            "entry.js",
            "import foo from \"npm:foo@^2\";\nconsole.log(foo.v);\n",
        );
        let opts = || BundleOptions {
            cwd: &cwd,
            entry: "entry.js",
            external: &[],
            minify: false,
            sourcemap: false,
        };
        let err = match bundle(opts()) {
            Err(e) => e,
            Ok(_) => panic!("expected a version-pin error for npm:foo@^2"),
        };
        assert!(
            err.contains("does not satisfy"),
            "expected a version-pin error, got: {err}"
        );

        // A satisfying pin still bundles.
        mk(
            &cwd,
            "entry.js",
            "import foo from \"npm:foo@^1\";\nconsole.log(foo.v);\n",
        );
        let b = bundle(opts()).unwrap();
        assert!(
            b.code.contains("module.exports") || b.code.contains("v:"),
            "{}",
            b.code
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn cjs_ambient_filename_is_shimmed_to_import_meta() {
        let cwd = scratch();
        mk(
            &cwd,
            "node_modules/uses-fn/package.json",
            r#"{"name":"uses-fn","version":"1.0.0","main":"index.js"}"#,
        );
        mk(
            &cwd,
            "node_modules/uses-fn/index.js",
            "exports.here = __filename;\nexports.dir = __dirname;\n",
        );
        mk(
            &cwd,
            "entry.js",
            "import * as m from \"uses-fn\";\nconsole.log(m.here, m.dir);\n",
        );
        let opts = BundleOptions {
            cwd: &cwd,
            entry: "entry.js",
            external: &[],
            minify: false,
            sourcemap: false,
        };
        let b = bundle(opts).unwrap();
        assert!(
            b.code.contains("import.meta.filename"),
            "expected the __filename shim:\n{}",
            b.code
        );
        assert!(
            b.code.contains("import.meta.dirname"),
            "expected the __dirname shim:\n{}",
            b.code
        );
        assert!(
            !b.code.contains("__filename") && !b.code.contains("__dirname"),
            "ambient names must be rewritten:\n{}",
            b.code
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn typescript_is_default_external_and_reported_for_embedding() {
        let cwd = scratch();
        mk(
            &cwd,
            "node_modules/typescript/package.json",
            r#"{"name":"typescript","version":"5.9.3","main":"lib/typescript.js"}"#,
        );
        mk(
            &cwd,
            "node_modules/typescript/lib/typescript.js",
            "module.exports = { marker: \"ts-inlined-marker\" };\n",
        );
        mk(
            &cwd,
            "entry.js",
            "import ts from \"typescript\";\nconsole.log(ts.marker);\n",
        );
        let opts = BundleOptions {
            cwd: &cwd,
            entry: "entry.js",
            external: &[],
            minify: false,
            sourcemap: false,
        };
        let b = bundle(opts).unwrap();
        assert_eq!(
            b.auto_embed,
            vec!["typescript".to_string()],
            "{:?}",
            b.auto_embed
        );
        assert!(
            !b.code.contains("ts-inlined-marker"),
            "default-external typescript must not be inlined:\n{}",
            b.code
        );
        assert!(
            b.code.contains("\"typescript\""),
            "expected an external import of typescript:\n{}",
            b.code
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn typescript_subpath_import_is_default_external() {
        let cwd = scratch();
        mk(
            &cwd,
            "node_modules/typescript/package.json",
            r#"{"name":"typescript","version":"5.9.3","main":"lib/typescript.js"}"#,
        );
        mk(
            &cwd,
            "node_modules/typescript/lib/tsserverlibrary.js",
            "module.exports = { marker: \"tsserver-inlined-marker\" };\n",
        );
        mk(
            &cwd,
            "entry.js",
            "import tss from \"typescript/lib/tsserverlibrary.js\";\nconsole.log(tss.marker);\n",
        );
        let opts = BundleOptions {
            cwd: &cwd,
            entry: "entry.js",
            external: &[],
            minify: false,
            sourcemap: false,
        };
        let b = bundle(opts).unwrap();
        assert_eq!(
            b.auto_embed,
            vec!["typescript".to_string()],
            "{:?}",
            b.auto_embed
        );
        assert!(
            !b.code.contains("tsserver-inlined-marker"),
            "default-external typescript subpath must not be inlined:\n{}",
            b.code
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn typescript_not_imported_is_not_auto_embedded() {
        let cwd = scratch();
        mk(
            &cwd,
            "node_modules/typescript/package.json",
            r#"{"name":"typescript","version":"5.9.3","main":"lib/typescript.js"}"#,
        );
        mk(
            &cwd,
            "node_modules/typescript/lib/typescript.js",
            "module.exports = {};\n",
        );
        mk(&cwd, "entry.js", "console.log(\"no ts here\");\n");
        let opts = BundleOptions {
            cwd: &cwd,
            entry: "entry.js",
            external: &[],
            minify: false,
            sourcemap: false,
        };
        let b = bundle(opts).unwrap();
        assert!(
            b.auto_embed.is_empty(),
            "unused typescript must not be auto-embedded: {:?}",
            b.auto_embed
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }
}

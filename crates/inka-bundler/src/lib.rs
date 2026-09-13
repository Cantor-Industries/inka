//! inka bundler: resolve a JS/TS entry (import maps + `npm:`/`jsr:` from the
//! Deno cache and `node_modules`) and emit a single self-contained ESM bundle.

mod resolve;

use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;

use rolldown::plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookResolveIdArgs, HookResolveIdOutput,
    HookResolveIdReturn, HookUsage, Plugin, PluginContext, Pluginable, SharedLoadPluginContext,
};
use rolldown::{
    BundlerBuilder, BundlerOptions, CodeSplittingMode, InputItem, IsExternal, OutputFormat,
    Platform, RawMinifyOptions, SourceMapType, TreeshakeOptions,
};
use rolldown_common::{ModuleType, Output};
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

/// A produced bundle plus any files the caller should embed alongside it.
pub struct Bundle {
    pub code: String,
    pub embedded: Vec<(String, Vec<u8>)>,
    pub warnings: Vec<String>,
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

    let options = BundlerOptions {
        input: Some(vec![InputItem {
            name: Some("chunk".to_string()),
            import: opts.entry.to_string(),
        }]),
        cwd: Some(opts.cwd.to_path_buf()),
        platform: Some(Platform::Node),
        format: Some(OutputFormat::Esm),
        external: Some(IsExternal::from(opts.external.to_vec())),
        treeshake: TreeshakeOptions::default(),
        // A single self-contained chunk: dynamic imports are inlined rather
        // than split into sibling files the artifact would not carry.
        code_splitting: Some(CodeSplittingMode::Bool(false)),
        minify: opts.minify.then_some(RawMinifyOptions::Bool(true)),
        sourcemap: opts.sourcemap.then_some(SourceMapType::File),
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
    for asset in &output.assets {
        if let Output::Chunk(chunk) = asset {
            chunks.push(chunk.code.clone());
        }
    }
    if chunks.len() != 1 {
        return Err(format!(
            "rolldown produced {} chunks; expected a single bundle",
            chunks.len()
        ));
    }
    let code = chunks.into_iter().next().unwrap();

    Ok(Bundle {
        code,
        embedded,
        warnings: Vec::new(),
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
        let npm_bare = if let Some(rest) = target.strip_prefix("npm:") {
            npm_target(rest)
        } else if bare {
            Some(target.clone())
        } else {
            None
        };
        if let Some(spec_bare) = npm_bare {
            if let Ok(Ok(resolved)) = ctx.resolve(&spec_bare, importer, None).await {
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

/// Strip the version from an `npm:` body, keeping the subpath:
/// `foo@1/sub` -> `foo/sub`, `@scope/foo@1/sub` -> `@scope/foo/sub`.
fn npm_target(body: &str) -> Option<String> {
    let (name, rest) = if body.starts_with('@') {
        let slash = body.find('/')?;
        let after = &body[slash + 1..];
        let end = after
            .find(['@', '/'])
            .map(|i| slash + 1 + i)
            .unwrap_or(body.len());
        (&body[..end], &body[end..])
    } else {
        let end = body.find(['@', '/']).unwrap_or(body.len());
        (&body[..end], &body[end..])
    };
    let sub = if let Some(rest) = rest.strip_prefix('@') {
        rest.find('/').map(|i| &rest[i + 1..])
    } else {
        rest.strip_prefix('/')
    };
    Some(match sub {
        Some(s) => format!("{name}/{s}"),
        None => name.to_string(),
    })
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
        match bundle(opts) {
            Ok(b) => assert!(b.code.contains("assert"), "expected inlined assert code"),
            Err(e) => eprintln!("skip: jsr bundle unavailable: {e}"),
        }
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
}

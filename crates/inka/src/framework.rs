// Framework detection for `inka desktop .`, vendored from Deno 2.9.7
// `cli/tools/framework.rs` (MIT, Copyright (c) the Deno authors).
//
// Detects web frameworks (Fresh, Astro, Remix, React Router, SvelteKit, Nuxt,
// SolidStart, TanStack Start, Vite) and generates the entrypoint + include
// paths so `inka desktop .` just works. Next.js is detected so we can give an
// actionable error: its server output can't be bundled into inka's single-file
// desktop payload.
//
// Adaptations from Deno's version: the build step runs the project's own
// `deno task build` / `npm run build` (Deno spawns its own binary), and the HMR
// command plumbing is dropped (inka's `--hmr` dev-runs an explicit entry).

use std::path::Path;
use std::path::PathBuf;

/// A dependency-free static file server used by the generated entrypoints.
/// inka's bundler is offline, so unlike Deno we don't import
/// `jsr:@std/http/file-server`; this keeps framework packaging working from a
/// cold Deno cache.
const STATIC_HELPER: &str = r#"const __types = { ".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8", ".mjs": "text/javascript; charset=utf-8", ".css": "text/css; charset=utf-8", ".json": "application/json; charset=utf-8", ".svg": "image/svg+xml", ".png": "image/png", ".jpg": "image/jpeg", ".jpeg": "image/jpeg", ".gif": "image/gif", ".ico": "image/x-icon", ".woff": "font/woff", ".woff2": "font/woff2", ".map": "application/json; charset=utf-8" };
async function __serveDir(req, fsRoot) {
  const url = new URL(req.url);
  let path = decodeURIComponent(url.pathname);
  if (path.endsWith("/")) path += "index.html";
  const target = fsRoot + path;
  if (!target.startsWith(fsRoot)) return new Response("not found", { status: 404 });
  try {
    const body = await Deno.readFile(target);
    const dot = path.lastIndexOf(".");
    const type = dot >= 0 ? __types[path.slice(dot).toLowerCase()] : undefined;
    return new Response(body, { headers: { "content-type": type ?? "application/octet-stream" } });
  } catch {
    return new Response("not found", { status: 404 });
  }
}
"#;

/// Result of framework detection.
#[derive(Debug)]
pub struct FrameworkDetection {
    /// Name of the detected framework (for display).
    pub name: &'static str,
    /// Generated entrypoint TypeScript/JavaScript code (production).
    pub entrypoint_code: String,
    /// Directories (relative to the project root) to ship in the payload.
    pub include_paths: Vec<String>,
    /// Whether the framework's build task must run before bundling.
    pub build: bool,
}

/// Entrypoint that boots the project's own Vite dev server *inside* the
/// desktop runtime for `inka desktop --hmr` (server code keeps `Deno.desktop`
/// access), adapted from Deno. Retained for framework dev-server HMR; inka's
/// `--hmr` currently runs an explicit entry file.
#[allow(dead_code)]
pub const VITE_DEV_ENTRYPOINT: &str = r#"// @ts-nocheck
import { createServer } from "vite";
const addr = Deno.env.get("DENO_SERVE_ADDRESS") ?? "";
const match = addr.match(/^tcp:(.+):(\d+)$/);
const server = await createServer({
  server: match
    ? { host: match[1], port: Number(match[2]), strictPort: true }
    : {},
});
await server.listen();
server.printUrls();
"#;

impl FrameworkDetection {
    /// Entrypoint that boots the framework's dev server inside the desktop
    /// runtime for `--hmr`, matching Deno (#35899). Only plain `vite dev`
    /// frameworks qualify.
    #[allow(dead_code)]
    pub fn hmr_entrypoint_code(&self) -> Option<&'static str> {
        match self.name {
            "Vite" | "SvelteKit" => Some(VITE_DEV_ENTRYPOINT),
            _ => None,
        }
    }

    /// Directories where the framework keeps static assets like favicons.
    pub fn static_asset_dirs(&self) -> &'static [&'static str] {
        match self.name {
            "Next.js" => &["public", "app", "src/app"],
            "Fresh" | "SvelteKit" => &["static"],
            _ => &["public"],
        }
    }
}

/// Search a framework's static asset directories for a favicon that can double
/// as the app icon (Linux PNGs only).
pub fn find_framework_favicon(dir: &Path, detection: &FrameworkDetection) -> Option<PathBuf> {
    let names = ["icon", "favicon", "apple-touch-icon", "logo"];
    for sub in detection.static_asset_dirs() {
        let base = dir.join(sub);
        if !base.is_dir() {
            continue;
        }
        for name in names {
            let candidate = base.join(format!("{name}.png"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Detect a web framework in `dir`.
pub fn detect_framework(dir: &Path) -> Result<Option<FrameworkDetection>, String> {
    // --- Config-file based detection (highest priority) ---
    if has_config_file(dir, "next.config") {
        return Ok(Some(detect_nextjs(dir)));
    }
    if dir.join("fresh.gen.ts").exists() || dir.join("_fresh").is_dir() {
        return Ok(Some(detect_fresh(dir)));
    }
    if has_config_file(dir, "astro.config") {
        return Ok(Some(detect_astro(dir)));
    }
    if has_config_file(dir, "nuxt.config") {
        return Ok(Some(detect_nitro_framework(dir, "Nuxt")));
    }

    let has_sveltekit = read_package_deps(dir)
        .map(|deps| deps.has("@sveltejs/kit") || deps.has_dev("@sveltejs/kit"))
        .unwrap_or(false);
    if has_config_file(dir, "svelte.config") && has_sveltekit {
        return detect_sveltekit(dir).map(Some);
    }

    // --- Package.json dependency-based detection ---
    if let Some(deps) = read_package_deps(dir) {
        if deps.has("@remix-run/react") || deps.has_dev("@remix-run/dev") {
            return Ok(Some(detect_remix(dir)));
        }
        if deps.has("@react-router/dev") || deps.has_dev("@react-router/dev") {
            return Ok(Some(detect_react_router(dir)));
        }
        if deps.has("@solidjs/start") {
            return Ok(Some(detect_nitro_framework(dir, "SolidStart")));
        }
        if deps.has("@tanstack/react-start") || deps.has("@tanstack/solid-start") {
            return Ok(Some(detect_nitro_framework(dir, "TanStack Start")));
        }
    }

    // --- Vite (lowest priority among bundlers) ---
    let has_vite_dep = read_package_deps(dir)
        .map(|deps| deps.has("vite") || deps.has_dev("vite"))
        .unwrap_or(false);
    if has_config_file(dir, "vite.config") || has_vite_dep {
        return Ok(Some(detect_vite(dir)));
    }

    // --- deno.json import-based detection ---
    if let Some(imports) = read_deno_json_imports(dir) {
        if imports
            .iter()
            .any(|i| i.starts_with("fresh") || i.starts_with("@fresh/core"))
        {
            return Ok(Some(detect_fresh(dir)));
        }
    }

    Ok(None)
}

// --- Framework-specific detection ---

fn detect_nextjs(_dir: &Path) -> FrameworkDetection {
    // Detected only so `build_framework` can report it clearly; the entrypoint
    // is unused. Next's server can't be bundled into inka's single-file
    // payload (native swc + worker processes).
    FrameworkDetection {
        name: "Next.js",
        entrypoint_code: String::new(),
        include_paths: Vec::new(),
        build: false,
    }
}

fn detect_astro(_dir: &Path) -> FrameworkDetection {
    FrameworkDetection {
        name: "Astro",
        entrypoint_code: "// @ts-nocheck\nimport \"./dist/server/entry.mjs\";\n".into(),
        include_paths: vec!["dist".into()],
        build: true,
    }
}

fn detect_fresh(dir: &Path) -> FrameworkDetection {
    let is_fresh2 = dir.join("_fresh/server.js").exists()
        || read_deno_json_imports(dir)
            .map(|imports| imports.iter().any(|i| i.starts_with("@fresh/core")))
            .unwrap_or(false);
    if is_fresh2 {
        let mut include_paths = vec!["_fresh".into()];
        if dir.join("static").is_dir() {
            include_paths.push("static".into());
        }
        FrameworkDetection {
            name: "Fresh",
            entrypoint_code: r#"// @ts-nocheck
const mod = await import("./_fresh/server.js");
Deno.serve(mod.default.fetch);
"#
            .into(),
            include_paths,
            build: true,
        }
    } else {
        FrameworkDetection {
            name: "Fresh",
            entrypoint_code: "// @ts-nocheck\nimport \"./main.ts\";\n".into(),
            include_paths: Vec::new(),
            build: false,
        }
    }
}

fn detect_remix(dir: &Path) -> FrameworkDetection {
    let mut include_paths = vec!["build".into()];
    if dir.join("public").is_dir() {
        include_paths.push("public".into());
    }
    FrameworkDetection {
        name: "Remix",
        entrypoint_code: "// @ts-nocheck\nimport \"./build/server/index.js\";\n".into(),
        include_paths,
        build: true,
    }
}

fn detect_react_router(dir: &Path) -> FrameworkDetection {
    let spa_mode = [
        "react-router.config.ts",
        "react-router.config.js",
        "react-router.config.mjs",
    ]
    .iter()
    .find_map(|f| std::fs::read_to_string(dir.join(f)).ok())
    .map(|text| {
        strip_comments(&text)
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .contains("ssr:false")
    })
    .unwrap_or(false);

    if spa_mode {
        let body = r#"const fsRoot = import.meta.dirname + "/build/client";
Deno.serve(async (req) => {
  const res = await __serveDir(req, fsRoot);
  if (
    res.status === 404 &&
    req.method === "GET" &&
    (req.headers.get("accept") ?? "").includes("text/html")
  ) {
    const index = new Request(new URL("/index.html", req.url), {
      headers: req.headers,
    });
    return await __serveDir(index, fsRoot);
  }
  return res;
});
"#;
        FrameworkDetection {
            name: "React Router",
            entrypoint_code: format!("// @ts-nocheck\n{STATIC_HELPER}{body}"),
            include_paths: vec!["build/client".into()],
            build: true,
        }
    } else {
        let body = r#"import { createRequestHandler } from "react-router";
import * as build from "./build/server/index.js";
const fsRoot = import.meta.dirname + "/build/client";
const handler = createRequestHandler(build, "production");
Deno.serve(async (req) => {
  if (req.method === "GET" || req.method === "HEAD") {
    const staticRes = await __serveDir(req, fsRoot);
    if (staticRes.status !== 404) {
      return staticRes;
    }
  }
  return await handler(req);
});
"#;
        FrameworkDetection {
            name: "React Router",
            entrypoint_code: format!("// @ts-nocheck\n{STATIC_HELPER}{body}"),
            include_paths: vec!["build".into()],
            build: true,
        }
    }
}

/// Remove `//` line comments and `/* */` block comments so a commented-out
/// setting isn't mistaken for the real one.
fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '/' {
            match chars.peek() {
                Some('/') => {
                    for c2 in chars.by_ref() {
                        if c2 == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                    continue;
                }
                Some('*') => {
                    chars.next();
                    let mut prev = '\0';
                    for c2 in chars.by_ref() {
                        if prev == '*' && c2 == '/' {
                            break;
                        }
                        prev = c2;
                    }
                    continue;
                }
                _ => {}
            }
        }
        out.push(c);
    }
    out
}

fn detect_sveltekit(dir: &Path) -> Result<FrameworkDetection, String> {
    let built = |entry: String, include: Vec<String>| FrameworkDetection {
        name: "SvelteKit",
        entrypoint_code: format!("// @ts-nocheck\n{entry}"),
        include_paths: include,
        build: true,
    };

    if dir.join(".deno-deploy/server.ts").exists() {
        return Ok(built(
            "import \"./.deno-deploy/server.ts\";\n".into(),
            vec![".deno-deploy".into()],
        ));
    }
    if dir.join(".output/server/index.ts").exists() {
        return Ok(built(
            "import \"./.output/server/index.ts\";\n".into(),
            vec![".output".into()],
        ));
    }
    if dir.join(".output/server/index.mjs").exists() {
        return Ok(built(
            "import \"./.output/server/index.mjs\";\n".into(),
            vec![".output".into()],
        ));
    }
    if dir.join("build/index.js").exists() && dir.join("build/handler.js").exists() {
        return Ok(built(
            "import \"./build/index.js\";\n".into(),
            sveltekit_build_includes(dir),
        ));
    }

    let config_text = read_config_text(dir, "svelte.config");
    if config_text.contains("@deno/svelte-adapter") {
        return Ok(built(
            "import \"./.deno-deploy/server.ts\";\n".into(),
            vec![".deno-deploy".into()],
        ));
    }
    if config_text.contains("svelte-adapter-deno") || config_text.contains("@sveltejs/adapter-node")
    {
        return Ok(built(
            "import \"./build/index.js\";\n".into(),
            vec!["build/client".into()],
        ));
    }
    if config_text.contains("nitro") {
        return Ok(built(
            "import \"./.output/server/index.mjs\";\n".into(),
            vec![".output".into()],
        ));
    }

    let auto_hint = if config_text.contains("adapter-auto") {
        "`@sveltejs/adapter-auto` produces no local server output, so there is \
         nothing to bundle. "
    } else {
        ""
    };
    Err(format!(
        "SvelteKit detected, but no adapter that `inka desktop` can bundle is \
         configured.\n{auto_hint}Configure a supported adapter in your \
         svelte.config:\n  - `@sveltejs/adapter-node`\n  - `@deno/svelte-adapter`"
    ))
}

fn sveltekit_build_includes(dir: &Path) -> Vec<String> {
    ["client", "static", "prerendered"]
        .iter()
        .map(|sub| format!("build/{sub}"))
        .filter(|rel| dir.join(rel).is_dir())
        .collect()
}

fn detect_nitro_framework(dir: &Path, name: &'static str) -> FrameworkDetection {
    let entry = if dir.join(".output/server/index.ts").exists() {
        "import \"./.output/server/index.ts\";\n"
    } else {
        "import \"./.output/server/index.mjs\";\n"
    };
    FrameworkDetection {
        name,
        entrypoint_code: format!("// @ts-nocheck\n{entry}"),
        include_paths: vec![".output".into()],
        build: true,
    }
}

fn detect_vite(dir: &Path) -> FrameworkDetection {
    if let Some(server_file) = ["server.js", "server.ts", "server.mjs"]
        .iter()
        .find(|f| dir.join(f).exists())
    {
        return FrameworkDetection {
            name: "Vite",
            entrypoint_code: format!("// @ts-nocheck\nimport \"./{server_file}\";\n"),
            include_paths: vec!["dist".into()],
            build: true,
        };
    }

    let body = r#"const fsRoot = import.meta.dirname + "/dist";
Deno.serve(async (req) => {
  const res = await __serveDir(req, fsRoot);
  if (
    res.status === 404 &&
    req.method === "GET" &&
    (req.headers.get("accept") ?? "").includes("text/html")
  ) {
    const index = new Request(new URL("/index.html", req.url), {
      headers: req.headers,
    });
    return await __serveDir(index, fsRoot);
  }
  return res;
});
"#;
    FrameworkDetection {
        name: "Vite",
        entrypoint_code: format!("// @ts-nocheck\n{STATIC_HELPER}{body}"),
        include_paths: vec!["dist".into()],
        build: true,
    }
}

// --- Helpers ---

/// Config-file extensions framework detection recognizes.
const CONFIG_EXTENSIONS: [&str; 5] = ["js", "mjs", "ts", "mts", "cjs"];

fn has_config_file(dir: &Path, base_name: &str) -> bool {
    CONFIG_EXTENSIONS
        .iter()
        .any(|ext| dir.join(format!("{base_name}.{ext}")).exists())
}

fn read_config_text(dir: &Path, base_name: &str) -> String {
    CONFIG_EXTENSIONS
        .iter()
        .find_map(|ext| std::fs::read_to_string(dir.join(format!("{base_name}.{ext}"))).ok())
        .unwrap_or_default()
}

struct PackageDeps {
    deps: serde_json::Value,
    dev_deps: serde_json::Value,
}

impl PackageDeps {
    fn has(&self, name: &str) -> bool {
        self.deps.get(name).is_some()
    }
    fn has_dev(&self, name: &str) -> bool {
        self.dev_deps.get(name).is_some()
    }
}

fn read_package_deps(dir: &Path) -> Option<PackageDeps> {
    let content = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let pkg: serde_json::Value = serde_json::from_str(&content).ok()?;
    Some(PackageDeps {
        deps: pkg
            .get("dependencies")
            .cloned()
            .unwrap_or(serde_json::Value::Object(Default::default())),
        dev_deps: pkg
            .get("devDependencies")
            .cloned()
            .unwrap_or(serde_json::Value::Object(Default::default())),
    })
}

/// Read the `imports` keys from deno.json / deno.jsonc (JSONC-aware).
fn read_deno_json_imports(dir: &Path) -> Option<Vec<String>> {
    let content = std::fs::read_to_string(dir.join("deno.json"))
        .or_else(|_| std::fs::read_to_string(dir.join("deno.jsonc")))
        .ok()?;
    let config = crate::config::parse_jsonc(&content).ok()?;
    let imports = config.get("imports")?.as_object()?;
    Some(imports.keys().cloned().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("inka-framework-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn no_framework_empty_dir() {
        let dir = tmp("empty");
        assert!(detect_framework(&dir).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_vite_spa() {
        let dir = tmp("vite");
        write(&dir, "vite.config.ts", "");
        write(&dir, "package.json", r#"{"devDependencies":{"vite":"^5"}}"#);
        let det = detect_framework(&dir).unwrap().unwrap();
        assert_eq!(det.name, "Vite");
        assert_eq!(det.include_paths, vec!["dist"]);
        assert!(det.build);
        assert!(det.entrypoint_code.contains("serveDir"));
        assert!(det.hmr_entrypoint_code().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_fresh2() {
        let dir = tmp("fresh2");
        std::fs::create_dir_all(dir.join("_fresh")).unwrap();
        write(&dir, "_fresh/server.js", "");
        let det = detect_framework(&dir).unwrap().unwrap();
        assert_eq!(det.name, "Fresh");
        assert!(det.entrypoint_code.contains("_fresh/server.js"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_astro() {
        let dir = tmp("astro");
        write(&dir, "astro.config.mjs", "");
        let det = detect_framework(&dir).unwrap().unwrap();
        assert_eq!(det.name, "Astro");
        assert!(det.entrypoint_code.contains("dist/server/entry.mjs"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_nitro_frameworks() {
        for (name, pkg) in [
            ("Nuxt", r#"{"dependencies":{"nuxt":"^3"}}"#),
            ("SolidStart", r#"{"dependencies":{"@solidjs/start":"^1"}}"#),
            (
                "TanStack Start",
                r#"{"dependencies":{"@tanstack/react-start":"^1"}}"#,
            ),
        ] {
            let dir = tmp(&name.to_lowercase().replace(' ', "-"));
            if name == "Nuxt" {
                write(&dir, "nuxt.config.ts", "");
            } else {
                write(&dir, "package.json", pkg);
            }
            let det = detect_framework(&dir).unwrap().unwrap();
            assert_eq!(det.name, name);
            assert_eq!(det.include_paths, vec![".output"]);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn detects_react_router_spa_and_ssr() {
        let dir = tmp("react-router");
        write(
            &dir,
            "package.json",
            r#"{"devDependencies":{"@react-router/dev":"^7","vite":"^5"}}"#,
        );
        write(&dir, "vite.config.ts", "");
        write(
            &dir,
            "react-router.config.ts",
            "export default { ssr: false };\n",
        );
        let det = detect_framework(&dir).unwrap().unwrap();
        assert_eq!(det.name, "React Router");
        assert_eq!(det.include_paths, vec!["build/client"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sveltekit_without_supported_adapter_errors() {
        let dir = tmp("sveltekit");
        write(
            &dir,
            "package.json",
            r#"{"devDependencies":{"@sveltejs/kit":"^2"}}"#,
        );
        write(
            &dir,
            "svelte.config.js",
            "import adapter from '@sveltejs/adapter-vercel';\n",
        );
        let err = detect_framework(&dir).unwrap_err();
        assert!(err.contains("SvelteKit"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

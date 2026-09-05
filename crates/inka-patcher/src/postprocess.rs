use anyhow::{bail, Context, Result};
use regex::Regex;

/// Node built-ins that can be imported statically as ESM. A default import mirrors
/// `require(b)` (module.exports); destructuring off it works for named access.
const BUILTINS: &[&str] = &[
    "assert", "async_hooks", "buffer", "child_process", "cluster", "console",
    "constants", "crypto", "dgram", "diagnostics_channel", "dns", "domain",
    "events", "fs", "http", "http2", "https", "inspector", "module", "net",
    "os", "path", "perf_hooks", "process", "punycode", "querystring",
    "readline", "repl", "stream", "string_decoder", "sys", "timers", "tls",
    "trace_events", "tty", "url", "util", "v8", "vm", "wasi",
    "worker_threads", "zlib",
];

const BUILTIN_SUBPATHS: &[&str] = &[
    "assert/strict",
    "dns/promises",
    "fs/promises",
    "path/posix",
    "path/win32",
    "readline/promises",
    "stream/consumers",
    "stream/promises",
    "stream/web",
    "timers/promises",
    "util/types",
];

/// Bare or `node:`-prefixed name of a Node built-in we can hoist to an ESM import.
/// Unknown node: submodules (e.g. `node:sqlite`) are intentionally NOT hoisted:
/// an unused ESM import would hard-fail the load, whereas a lazy require only
/// throws if that optional feature is actually used.
fn builtin_name(raw: &str) -> Option<&str> {
    let bare = raw.strip_prefix("node:").unwrap_or(raw);
    (BUILTINS.contains(&bare) || BUILTIN_SUBPATHS.contains(&bare)).then_some(bare)
}

fn import_var(name: &str) -> String {
    format!("__rq_{}", name.replace(['-', '/'], "_"))
}

/// Make a rolldown ESM bundle engine-viable. The engine cannot run `createRequire`
/// (its deno `require` ops need the Option-B node-services stack), so:
///   1. hoist each `__require("<node builtin>")` to a static default ESM import and
///      rewrite the call site (default == module.exports mirrors require());
///   2. delete the `createRequire` import and redefine `__require` to throw a
///      catchable JS Error — optional natives (bufferutil etc.) then fall through
///      the package's own try/catch to its pure-JS path;
///   3. neutralize the `process.env.<knob>` reads listed in the spec (deny-by-default
///      env access would otherwise force allow-env on the artifact).
pub fn postprocess(code: &str, neutralize_env: &[String]) -> Result<String> {
    let re = Regex::new(r#"__require\("([^"]+)"\)"#).unwrap();
    let mut used: Vec<&str> = re
        .captures_iter(code)
        .filter_map(|c| c.get(1).map(|m| m.as_str()))
        .filter_map(|n| builtin_name(n))
        .collect();
    used.sort_unstable();
    used.dedup();

    let mut out = String::new();
    for bare in &used {
        out.push_str(&format!("import {} from \"node:{bare}\";\n", import_var(bare)));
    }
    out.push_str(code);

    for bare in &used {
        // replace both bare and node:-prefixed call sites
        out = out.replace(&format!("__require(\"{bare}\")"), &import_var(bare));
        out = out.replace(&format!("__require(\"node:{bare}\")"), &import_var(bare));
    }

    // Replace rolldown's `var __require = createRequire(import.meta.url)()` shim with a
    // throwing (catchable) one. If rolldown emitted no such shim there are no external
    // requires to shim — unless __require is still referenced (then the format changed).
    let has_define = out
        .lines()
        .any(|l| l.contains("var __require =") && l.contains("createRequire(import.meta.url)"));
    if has_define {
        let mut lines: Vec<String> = out.lines().map(str::to_owned).collect();
        for line in lines.iter_mut() {
            if line.contains("var __require =") && line.contains("createRequire(import.meta.url)") {
                *line = [
                    "var __require = /* @__PURE__ */ ((x) => {",
                    "  const e = new Error('[inka-patch] dynamic require is not supported: ' + x);",
                    "  throw e;",
                    "});",
                ]
                .join("\n");
                break;
            }
        }
        out = lines.join("\n");
    } else if out.contains("__require(") {
        bail!("postprocess: external requires remain but rolldown emitted no __require define line (format changed?)");
    }

    let mut out = out.replace("import { createRequire } from \"node:module\";\n", "");

    if out.contains("createRequire") {
        bail!("postprocess: createRequire still referenced after rewrite");
    }

    for knob in neutralize_env {
        if !knob.is_empty() {
            let needle = format!("process.env.{knob}");
            if !out.contains(&needle) {
                bail!("postprocess: env knob {knob} not present (source changed?)");
            }
            out = out.replace(&needle, "\"1\"");
        }
    }

    Ok(out)
}

/// Rewrite `package.json` so the resolver's `import` condition serves the ESM build.
pub fn point_exports_at_esm(pkg_dir: &std::path::Path, outfile: &str) -> Result<()> {
    let path = pkg_dir.join("package.json");
    let raw = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let mut v: serde_json::Value = serde_json::from_slice(&raw)
        .with_context(|| format!("parse {}", path.display()))?;
    let target = format!("./{outfile}");

    let mut exports = serde_json::Map::new();
    if let Some(old) = v.get("exports").and_then(serde_json::Value::as_object) {
        for (k, val) in old {
            if k != "." {
                exports.insert(k.clone(), val.clone());
            }
        }
    }
    let mut dot = serde_json::Map::new();
    dot.insert("import".to_string(), serde_json::Value::String(target.clone()));
    dot.insert("default".to_string(), serde_json::Value::String(target));
    exports.insert(".".to_string(), serde_json::Value::Object(dot));
    v["exports"] = serde_json::Value::Object(exports);

    let mut pretty = serde_json::to_string_pretty(&v)?;
    pretty.push('\n');
    std::fs::write(&path, pretty).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> String {
        // Mimic rolldown ESM output shape (single-line __require define, CJS helpers).
        [
            "import { createRequire } from \"node:module\";",
            "//#region \\0rolldown/runtime.js",
            "var __require = /* #__PURE__ */ (() => createRequire(import.meta.url))();",
            "var __commonJSMin = (cb, mod) => () => (mod || (cb((mod = { exports: {} }).exports, mod), cb = null), mod.exports);",
            "//#endregion",
            "var zlib = __require(\"zlib\");",
            "const { randomFillSync } = __require(\"crypto\");",
            "try {",
            "\tconst bufferUtil = __require(\"bufferutil\");",
            "} catch (e) { /* optional */ }",
            "if (!process.env.WS_NO_BUFFER_UTIL) { runNative(); }",
            "export { WebSocket, WebSocketServer };",
        ]
        .join("\n")
    }

    #[test]
    fn rewrites_builtins_env_and_require() {
        let out = postprocess(&sample(), &["WS_NO_BUFFER_UTIL".to_string()]).unwrap();
        assert!(out.contains("import __rq_crypto from \"node:crypto\";\n"), "crypto import:\n{out}");
        assert!(out.contains("import __rq_zlib from \"node:zlib\";\n"));
        assert!(out.contains("var zlib = __rq_zlib;"));
        assert!(out.contains("const { randomFillSync } = __rq_crypto;"));
        assert!(!out.contains("createRequire"));
        assert!(out.contains("__require(\"bufferutil\")"), "native require preserved:\n{out}");
        assert!(out.contains("[inka-patch] dynamic require"));
        assert!(!out.contains("process.env.WS_NO_BUFFER_UTIL"));
        assert!(out.contains("if (!\"1\") { runNative(); }"));
    }

    #[test]
    fn rejects_unknown_env_knob() {
        assert!(postprocess(&sample(), &["WS_NOPE".to_string()]).is_err());
    }

    #[test]
    fn rejects_missing_require_define() {
        let no_def = sample().replace("createRequire(import.meta.url)", "other()");
        assert!(postprocess(&no_def, &[]).is_err());
    }

    #[test]
    fn handles_node_prefixed_builtin_requires() {
        let src = [
            "import { createRequire } from \"node:module\";",
            "var __require = /* #__PURE__ */ (() => createRequire(import.meta.url))();",
            "var assert = __require(\"node:assert\");",
            "var stream = __require(\"stream\");",
            "var { writeFile } = __require(\"node:fs/promises\");",
            "export default { assert, stream, writeFile };",
        ]
        .join("\n");
        let out = postprocess(&src, &[]).unwrap();
        assert!(out.contains("import __rq_assert from \"node:assert\";\n"));
        assert!(out.contains("import __rq_stream from \"node:stream\";\n"));
        assert!(out.contains("import __rq_fs_promises from \"node:fs/promises\";\n"));
        assert!(out.contains("var assert = __rq_assert;"));
        assert!(out.contains("var stream = __rq_stream;"));
        assert!(out.contains("var { writeFile } = __rq_fs_promises;"));
        assert!(!out.contains("createRequire"));
        assert!(!out.contains("__require(\"node:assert\")"));
    }

    #[test]
    fn leaves_unknown_node_submodule_as_lazy_require() {
        let src = [
            "import { createRequire } from \"node:module\";",
            "var __require = /* #__PURE__ */ (() => createRequire(import.meta.url))();",
            "var fs = __require(\"node:fs\");",
            "const db = () => __require(\"node:sqlite\");",
            "export default { fs, db };",
        ]
        .join("\n");
        let out = postprocess(&src, &[]).unwrap();
        assert!(out.contains("var fs = __rq_fs;"));
        assert!(out.contains("__require(\"node:sqlite\")"), "lazy unknown kept");
        assert!(!out.contains("createRequire"));
    }

    #[test]
    fn no_externals_needs_no_require_shim() {
        let src = "import { Mime } from './Mime.js';\nexport default new Mime();\n";
        let out = postprocess(src, &[]).unwrap();
        assert_eq!(out, src);
    }
}

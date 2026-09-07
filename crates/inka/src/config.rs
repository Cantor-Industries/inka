// inka build config: synthesize the artifact manifest from package.json /
// deno.json(.jsonc) so a `*.manifest` file is no longer required.
//
// Sources (all optional, top-level fields):
//   package.json : "permissions" (Deno shape, named sets), "inka" { runtime,
//                  "tested-against" }
//   deno.json(.c): same top-level fields, plus "compile"."permissions"
// When both files exist deno.json wins per-key (deno.jsonc = comment/trailing
// comma tolerant).
//
// Only the launcher-recognized manifest keys are emitted:
//   runtime=inka_runtime<spec>, tested-against=<v>,
//   allow-<cat>=<list> / deny-<cat>=<list>   (cat: read|write|net|env|run|sys|ffi)

use std::path::Path;

use serde_json::Value;

const CATEGORIES: [&str; 7] = ["read", "write", "net", "env", "run", "sys", "ffi"];

/// Strip `//` and `/* … */` comments and trailing commas from JSONC text,
/// respecting string literals and escapes.
pub fn strip_jsonc(input: &str) -> String {
    let bytes: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    let n = bytes.len();
    let mut in_string = false;
    let mut prev_non_ws: char = '\0';
    while i < n {
        let c = bytes[i];
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < n {
                out.push(bytes[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
                i += 1;
            }
            '/' if i + 1 < n && bytes[i + 1] == '/' => {
                while i < n && bytes[i] != '\n' {
                    i += 1;
                }
            }
            '/' if i + 1 < n && bytes[i + 1] == '*' => {
                i += 2;
                while i + 1 < n && !(bytes[i] == '*' && bytes[i + 1] == '/') {
                    i += 1;
                }
                i = (i + 2).min(n);
            }
            ',' => {
                // peek ahead for a closing bracket
                let mut j = i + 1;
                while j < n && bytes[j].is_whitespace() {
                    j += 1;
                }
                if j < n && (bytes[j] == '}' || bytes[j] == ']') {
                    // drop the trailing comma
                    i += 1;
                } else {
                    out.push(c);
                    i += 1;
                }
            }
            _ => {
                if !c.is_whitespace() {
                    prev_non_ws = c;
                }
                let _ = prev_non_ws;
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn read_config_file(cwd: &Path, name: &str) -> Option<(String, Value)> {
    let path = cwd.join(name);
    let raw = std::fs::read_to_string(&path).ok()?;
    let text = if name.ends_with(".jsonc") {
        strip_jsonc(&raw)
    } else {
        raw
    };
    let value = serde_json::from_str(&text).ok()?;
    Some((path.display().to_string(), value))
}

/// Loaded project config files (highest-first for merge).
struct ConfigFiles {
    pkg: Option<Value>,
    deno: Option<Value>,
}

fn load(cwd: &Path) -> ConfigFiles {
    let pkg = read_config_file(cwd, "package.json").map(|(_, v)| v);
    // prefer deno.json; fall back to deno.jsonc
    let deno = read_config_file(cwd, "deno.json")
        .or_else(|| read_config_file(cwd, "deno.jsonc"))
        .map(|(_, v)| v);
    ConfigFiles { pkg, deno }
}

/// The effective runtime/tested-against from the config (deno wins).
fn inka_block_runtime(cfg: &ConfigFiles) -> (Option<String>, Option<String>) {
    let mut runtime = None;
    let mut tested = None;
    for file in [&cfg.pkg, &cfg.deno].into_iter().flatten() {
        if let Some(blk) = file.get("inka") {
            if runtime.is_none() {
                runtime = blk
                    .get("runtime")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            if tested.is_none() {
                tested = blk
                    .get("tested-against")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
        }
    }
    // deno wins over package.json
    if let Some(blk) = cfg
        .deno
        .as_ref()
        .and_then(|d| d.get("inka"))
    {
        if let Some(r) = blk.get("runtime").and_then(Value::as_str) {
            runtime = Some(r.to_string());
        }
        if let Some(t) = blk.get("tested-against").and_then(Value::as_str) {
            tested = Some(t.to_string());
        }
    }
    (runtime, tested)
}

/// Render an allow/deny value: `true` => "*", array => comma list.
fn render_val(v: &Value) -> Option<String> {
    match v {
        Value::Bool(true) => Some("*".to_string()),
        Value::Bool(false) => None,
        Value::String(s) => Some(s.to_string()),
        Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .filter_map(|i| i.as_str().map(str::to_string))
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(","))
            }
        }
        _ => None,
    }
}

/// Apply one category map (`{ cat: bool | array | {allow,deny,ignore} }`) into
/// aggregated allow/deny lists. deno has no `ignore`/`import` inka equivalent.
fn apply_category_map(map: &Value, allow: &mut Vec<(String, String)>, deny: &mut Vec<(String, String)>, warns: &mut Vec<String>) {
    let Some(obj) = map.as_object() else { return };
    for (cat, val) in obj {
        if !CATEGORIES.contains(&cat.as_str()) {
            if cat == "import" {
                warns.push(format!(
                    "permission category 'import' has no inka equivalent; ignored"
                ));
            } else {
                warns.push(format!("unknown permission category '{cat}'; ignored"));
            }
            continue;
        }
        match val {
            Value::Bool(b) => {
                if *b {
                    allow.push((cat.clone(), "*".to_string()));
                }
            }
            Value::Array(_) => {
                if let Some(list) = render_val(val) {
                    allow.push((cat.clone(), list));
                }
            }
            Value::Object(o) => {
                if let Some(a) = o.get("allow").and_then(render_val) {
                    allow.push((cat.clone(), a));
                }
                if let Some(d) = o.get("deny").and_then(render_val) {
                    deny.push((cat.clone(), d));
                }
                if o.contains_key("ignore") {
                    warns.push(format!(
                        "permission '{cat}' 'ignore' has no inka equivalent; skipped"
                    ));
                }
            }
            _ => {}
        }
    }
}

/// Pick the category map to use for the build:
///   - `-P <name>`: named set from deno.json (then package.json),
///   - else deno.json `compile.permissions` (direct map, or a set name),
///   - else the `default` set (deno.json wins per-key over package.json).
fn effective_permission_map(
    cfg: &ConfigFiles,
    perm_set: Option<&str>,
) -> Option<Value> {
    let named_set = |file: Option<&Value>, name: &str| -> Option<Value> {
        let perms = file?.get("permissions")?.as_object()?;
        perms.get(name).cloned()
    };
    if let Some(name) = perm_set {
        let d = named_set(cfg.deno.as_ref(), name);
        let p = named_set(cfg.pkg.as_ref(), name);
        return d.or(p);
    }
    // compile.permissions (deno compile analog) takes precedence over default
    if let Some(compile) = cfg
        .deno
        .as_ref()
        .and_then(|d| d.get("compile"))
    {
        if let Some(p) = compile.get("permissions") {
            if p.is_object() {
                return Some(p.clone());
            }
            if let Some(name) = p.as_str() {
                if let Some(s) = named_set(cfg.deno.as_ref(), name) {
                    return Some(s);
                }
            }
        }
    }
    // merge default set: start from package.json, override per-key from deno.json
    let p = named_set(cfg.pkg.as_ref(), "default").unwrap_or_else(|| Value::Object(Default::default()));
    let d = named_set(cfg.deno.as_ref(), "default");
    let mut merged = p;
    if let (Some(dv), Some(mo)) = (d, merged.as_object_mut()) {
        for (k, v) in dv.as_object().unwrap_or(&serde_json::Map::new()) {
            mo.insert(k.clone(), v.clone());
        }
    }
    (!merged.as_object().map(|o| o.is_empty()).unwrap_or(true)).then_some(merged)
}

/// Synthesize manifest bytes from project config (no defaults applied yet).
/// Returns the lines; caller applies the runtime floor default + module= later.
pub struct Synth {
    pub bytes: Vec<u8>,
    pub warnings: Vec<String>,
}

pub fn synthesize_manifest(cwd: &Path, perm_set: Option<&str>) -> Synth {
    let cfg = load(cwd);
    let mut lines: Vec<String> = Vec::new();
    let mut warns: Vec<String> = Vec::new();

    let (runtime, tested) = inka_block_runtime(&cfg);
    if let Some(r) = runtime {
        let line = if r.starts_with('>') || r.starts_with('=') {
            format!("runtime=inka_runtime{r}")
        } else {
            format!("runtime=inka_runtime=={r}")
        };
        lines.push(line);
    }
    if let Some(t) = tested {
        lines.push(format!("tested-against={t}"));
    }

    if let Some(map) = effective_permission_map(&cfg, perm_set) {
        let mut allow: Vec<(String, String)> = Vec::new();
        let mut deny: Vec<(String, String)> = Vec::new();
        apply_category_map(&map, &mut allow, &mut deny, &mut warns);
        for (cat, list) in allow {
            lines.push(format!("allow-{cat}={list}"));
        }
        for (cat, list) in deny {
            lines.push(format!("deny-{cat}={list}"));
        }
    }

    let mut bytes = String::new();
    if !lines.is_empty() {
        bytes.push_str(&lines.join("\n"));
        bytes.push('\n');
    }
    Synth {
        bytes: bytes.into_bytes(),
        warnings: warns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write(cwd: &Path, name: &str, content: &str) {
        std::fs::create_dir_all(cwd).unwrap();
        std::fs::write(cwd.join(name), content).unwrap();
    }

    fn read_manifest(cwd: &Path, perm_set: Option<&str>) -> String {
        let s = synthesize_manifest(cwd, perm_set);
        String::from_utf8(s.bytes).unwrap()
    }

    #[test]
    fn jsonc_strip() {
        let s = strip_jsonc(
            "{\n  // a comment\n  \"imports\": {\"a\": \"b\",}, /* block */ \"n\": 1,}",
        );
        assert!(serde_json::from_str::<Value>(&s).is_ok(), "json: {s}");
    }

    #[test]
    fn empty_config_emits_nothing() {
        let cwd = PathBuf::from("/tmp/inkaconf-empty");
        let _ = std::fs::remove_dir_all(&cwd);
        std::fs::create_dir_all(&cwd).unwrap();
        let s = read_manifest(&cwd, None);
        assert_eq!(s, "");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn deno_permissions_map() {
        let cwd = PathBuf::from("/tmp/inkaconf-deno");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{
  "permissions": {
    "default": {
      "read": ["./data", "/etc"],
      "env": { "allow": true, "ignore": ["API_KEY"] },
      "net": ["127.0.0.1"],
      "write": false,
      "import": true,
      "ffi": { "deny": ["libc.so"] }
    }
  }
}"#,
        );
        let s = read_manifest(&cwd, None);
        assert!(s.contains("allow-read=./data,/etc"), "{s}");
        assert!(s.contains("allow-env=*"), "{s}");
        assert!(s.contains("allow-net=127.0.0.1"), "{s}");
        assert!(!s.contains("allow-write"), "{s}");
        assert!(!s.contains("allow-import"), "{s}");
        assert!(s.contains("deny-ffi=libc.so"), "{s}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn deno_beats_package_and_named_set() {
        let cwd = PathBuf::from("/tmp/inkaconf-both");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "package.json",
            r#"{
  "name": "x",
  "permissions": { "default": { "read": ["./pkg"] } },
  "inka": { "runtime": ">=0.266.0" }
}"#,
        );
        write(
            &cwd,
            "deno.json",
            r#"{
  "permissions": {
    "default": { "read": ["./deno"], "net": true },
    "server": { "net": ["0.0.0.0:80"] }
  },
  "inka": { "tested-against": "0.266.0" }
}"#,
        );
        let s = read_manifest(&cwd, None);
        assert!(s.contains("allow-read=./deno"), "deno read wins: {s}");
        assert!(s.contains("allow-net=*"), "{s}");
        assert!(s.contains("runtime=inka_runtime>=0.266.0"), "{s}");
        assert!(s.contains("tested-against=0.266.0"), "{s}");
        let s2 = read_manifest(&cwd, Some("server"));
        assert!(s2.contains("allow-net=0.0.0.0:80"), "{s2}");
        assert!(!s2.contains("allow-read"), "{s2}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn compile_permissions_and_runtime_default_exact() {
        let cwd = PathBuf::from("/tmp/inkaconf-compile");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{
  "permissions": { "default": { "read": ["./data"] } },
  "compile": {
    "permissions": { "env": true }
  },
  "inka": { "runtime": "0.270.0" }
}"#,
        );
        let s = read_manifest(&cwd, None);
        assert!(s.contains("allow-env=*"), "compile perms win: {s}");
        assert!(!s.contains("allow-read"), "{s}");
        assert!(s.contains("runtime=inka_runtime==0.270.0"), "{s}");
        let _ = std::fs::remove_dir_all(&cwd);
    }
}

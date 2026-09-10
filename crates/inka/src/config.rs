// inka build config: synthesize the artifact manifest from package.json /
// deno.json(.jsonc) so a `*.manifest` file is no longer required.
//
// Sources (all optional, top-level fields):
//   package.json : "permissions" (Deno shape, named sets), "inka" { runtime,
//                  "tested-against", "permissions" (marker naming a set) }
//   deno.json(.c): same top-level fields, plus "compile"."permissions"
//                  (deno.jsonc = comment/trailing comma tolerant).
// When both files exist deno.json wins per-key.
//
// Permission baking honors Deno's threat model: a plain `permissions.default`
// is *dev-run* intent (it exists so `deno run -P` / `deno task` work) and is
// NEVER baked. Only explicit build-intent sources bake, in precedence order:
//   -P <set>  >  deno.json compile.permissions  >  inka.permissions marker.
// With no source selected the artifact is deny-by-default (no allow/deny
// lines), matching the manifest-less posture.
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

/// Outcome of probing one config file. An *absent* file is simply "no config";
/// a file that exists but cannot be read or parsed is surfaced as a warning
/// (never silently treated as absent).
enum LoadOutcome {
    Missing,
    Ok(Value),
    Unreadable(String),
    Unparseable(String),
}

fn read_config_file(cwd: &Path, name: &str) -> LoadOutcome {
    let path = cwd.join(name);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LoadOutcome::Missing,
        Err(e) => {
            return LoadOutcome::Unreadable(format!(
                "{} exists but cannot be read: {e}",
                path.display()
            ))
        }
    };
    // JSONC stripping happens first; a parse failure of the stripped text is an
    // Unparseable case, not a read error.
    let text = if name.ends_with(".jsonc") {
        strip_jsonc(&raw)
    } else {
        raw
    };
    match serde_json::from_str(&text) {
        Ok(v) => LoadOutcome::Ok(v),
        Err(e) => LoadOutcome::Unparseable(format!(
            "{} is not valid JSON: {e}",
            path.display()
        )),
    }
}

/// Probe deno.jsonc as a fallback, recording any warning. Returns the parsed
/// value if deno.jsonc is usable, otherwise None.
fn read_deno_jsonc(cwd: &Path, warns: &mut Vec<String>) -> Option<Value> {
    match read_config_file(cwd, "deno.jsonc") {
        LoadOutcome::Ok(v) => Some(v),
        LoadOutcome::Missing => None,
        LoadOutcome::Unreadable(m) | LoadOutcome::Unparseable(m) => {
            warns.push(m);
            None
        }
    }
}

/// Loaded project config files (highest-first for merge).
struct ConfigFiles {
    pkg: Option<Value>,
    deno: Option<Value>,
}

fn load(cwd: &Path) -> (ConfigFiles, Vec<String>) {
    let mut warns: Vec<String> = Vec::new();

    // package.json is a single source.
    let pkg = match read_config_file(cwd, "package.json") {
        LoadOutcome::Ok(v) => Some(v),
        LoadOutcome::Missing => None,
        LoadOutcome::Unreadable(m) | LoadOutcome::Unparseable(m) => {
            warns.push(m);
            None
        }
    };

    // deno.json is preferred; deno.jsonc is only consulted when deno.json is
    // absent *or* unusable (a bad deno.json falls through to deno.jsonc, as the
    // old "treat parse failure as no config" path did — but now it is loud).
    let deno = match read_config_file(cwd, "deno.json") {
        LoadOutcome::Ok(v) => Some(v),
        LoadOutcome::Missing => read_deno_jsonc(cwd, &mut warns),
        LoadOutcome::Unreadable(m) | LoadOutcome::Unparseable(m) => {
            warns.push(m);
            read_deno_jsonc(cwd, &mut warns)
        }
    };

    (ConfigFiles { pkg, deno }, warns)
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

/// Apply one category map (`{ cat: bool | string | array | {allow,deny,ignore} }`)
/// into aggregated allow/deny lists. deno has no `ignore`/`import` inka equivalent.
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
            Value::String(s) => {
                if !s.is_empty() {
                    allow.push((cat.clone(), s.clone()));
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

/// True when a permission descriptor looks like a relative filesystem path
/// (not `*`, not absolute, not a URL/scheme).
fn is_relative_path(item: &str) -> bool {
    !item.is_empty()
        && item != "*"
        && !item.starts_with('/')
        && !item.contains("://")
}

/// Append the scalar string items of a category value (`true` counts as `*`).
fn collect_permission_items(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => {
            for i in items {
                if let Some(s) = i.as_str() {
                    out.push(s.to_string());
                }
            }
        }
        Value::Bool(true) => out.push("*".to_string()),
        _ => {}
    }
}

/// Warn about category values that will not survive the permission DSL:
///  - relative `read`/`write` grants resolve at run time against the launch
///    directory, not the build directory;
///  - an item containing a comma or newline cannot be represented (comma is the
///    list separator; the manifest is line-oriented).
fn validate_category_map(map: &Value, warns: &mut Vec<String>) {
    let Some(obj) = map.as_object() else { return };
    for (cat, val) in obj {
        let mut items: Vec<String> = Vec::new();
        match val {
            Value::Object(o) => {
                if let Some(a) = o.get("allow") {
                    collect_permission_items(a, &mut items);
                }
                if let Some(d) = o.get("deny") {
                    collect_permission_items(d, &mut items);
                }
            }
            other => collect_permission_items(other, &mut items),
        }
        for item in items {
            if (cat == "read" || cat == "write") && is_relative_path(&item) {
                warns.push(format!(
                    "permission '{cat}' grants relative path '{item}'; it resolves at run time \
                     against the launch directory, not the build directory (use an absolute path \
                     to pin it)"
                ));
            }
            if item.contains(',') || item.contains('\n') {
                warns.push(format!(
                    "permission item '{item}' contains a comma or newline, which the permission \
                     DSL cannot represent; the grant may be malformed"
                ));
            }
        }
    }
}

/// Warn about `deny-<cat>` entries that cannot trim anything because the
/// category was never allowed (the artifact is deny-by-default).
fn warn_ineffective_denies(
    allow: &[(String, String)],
    deny: &[(String, String)],
    warns: &mut Vec<String>,
) {
    for (cat, _) in deny {
        if !allow.iter().any(|(a, _)| a == cat) {
            warns.push(format!(
                "deny-{cat} has no matching allow-{cat} (or permissions=all); the artifact is \
                 deny-by-default, so the deny has no effect"
            ));
        }
    }
}

/// Look up a named permission set inside one config file's `permissions`
/// table. Returns `None` when the file, the table, or the named set is absent.
fn named_set_in(file: Option<&Value>, name: &str) -> Option<Value> {
    let sets = file?.get("permissions")?;
    sets.as_object()?.get(name).cloned()
}

/// Resolve an explicitly-selected permission set NAME across both files. When
/// both package.json and deno.json define the set, merge per-category with
/// deno.json winning each key. When it exists in neither file, record a
/// warning and treat it as "no permission source" (deny-by-default artifact) —
/// never silently fall back to something else.
fn resolve_named_set(
    cfg: &ConfigFiles,
    name: &str,
    source: &str,
    notes: &mut Vec<String>,
) -> Option<Value> {
    let d = named_set_in(cfg.deno.as_ref(), name);
    let p = named_set_in(cfg.pkg.as_ref(), name);
    match (d, p) {
        (Some(dv), Some(pv)) => {
            let mut merged = pv;
            if let (Some(dobj), Some(mobj)) = (dv.as_object(), merged.as_object_mut()) {
                for (k, v) in dobj {
                    mobj.insert(k.clone(), v.clone());
                }
            }
            Some(merged)
        }
        (Some(dv), None) => Some(dv),
        (None, Some(pv)) => Some(pv),
        (None, None) => {
            notes.push(format!(
                "{source} names permission set '{name}', which is not defined in \
                 deno.json or package.json permissions; the artifact is deny-by-default"
            ));
            None
        }
    }
}

/// Would a single category value actually grant anything if baked? Mirrors
/// what `apply_category_map` emits as an `allow-*` line.
fn grants_access(v: &Value) -> bool {
    match v {
        Value::Bool(true) => true,
        Value::String(s) => !s.is_empty(),
        Value::Array(items) => items.iter().any(Value::is_string),
        Value::Object(o) => o.get("allow").and_then(render_val).is_some(),
        _ => false,
    }
}

/// Does a permission set contain any category that would actually grant access?
/// (A set of deny-only or empty entries grants nothing, so it is not "declared
/// permissions" worth warning about.)
fn set_declares_grants(set: &Value) -> bool {
    set.as_object()
        .map(|o| o.values().any(grants_access))
        .unwrap_or(false)
}

/// Pick the permission source to bake for the build, plus informational notes.
/// Only explicit build-intent sources are ever baked, in this order:
///   1. CLI `-P/--permission-set <name>` — a named set.
///   2. deno.json `compile.permissions` — the deno-compile analog; either a
///      direct category map, or a string naming a set. `inka build` *is* the
///      compile step, so this bakes automatically (documented divergence from
///      Deno, which requires `-P` even for compile permissions).
///   3. An `inka.permissions` marker (deno.json wins over package.json) whose
///      value is a set-name string.
/// A plain `permissions.default` set with none of the above markers is dev-run
/// intent and is IGNORED; if such a set would actually grant something, an
/// informational note is returned so the silent drop is never invisible.
/// Unknown or malformed explicit sources warn and yield a deny-by-default
/// artifact (no `allow-*`/`deny-*` lines).
fn effective_permission_map(
    cfg: &ConfigFiles,
    perm_set: Option<&str>,
) -> (Option<Value>, Vec<String>) {
    let mut notes: Vec<String> = Vec::new();

    // 1. explicit CLI set selection
    if let Some(name) = perm_set {
        return (resolve_named_set(cfg, name, "-P", &mut notes), notes);
    }

    // 2. deno.json compile.permissions (deno-compile analog)
    if let Some(compile) = cfg.deno.as_ref().and_then(|d| d.get("compile")) {
        if let Some(p) = compile.get("permissions") {
            if p.is_object() {
                return (Some(p.clone()), notes);
            }
            if let Some(name) = p.as_str() {
                return (
                    resolve_named_set(cfg, name, "compile.permissions", &mut notes),
                    notes,
                );
            }
            notes.push(
                "compile.permissions must be a permissions map or a set-name \
                 string; the artifact is deny-by-default"
                    .to_string(),
            );
            return (None, notes);
        }
    }

    // 3. inka.permissions marker (deno.json wins over package.json). Read the
    //    raw value so a present-but-malformed marker is distinguishable from
    //    an absent one.
    let deno_marker = cfg
        .deno
        .as_ref()
        .and_then(|d| d.get("inka"))
        .and_then(|i| i.get("permissions"));
    let pkg_marker = cfg
        .pkg
        .as_ref()
        .and_then(|p| p.get("inka"))
        .and_then(|i| i.get("permissions"));
    match deno_marker.or(pkg_marker) {
        Some(v) => match v.as_str() {
            Some(name) => {
                return (
                    resolve_named_set(cfg, name, "inka.permissions", &mut notes),
                    notes,
                )
            }
            None => {
                notes.push(
                    "inka.permissions must name a permission set (string); \
                     the artifact is deny-by-default"
                        .to_string(),
                );
                return (None, notes);
            }
        },
        None => {}
    }

    // 4. Nothing was selected. A plain `permissions.default` is dev-run intent
    //    and must not leak into the artifact; surface it so the drop is visible.
    let mut declaring: Vec<&str> = Vec::new();
    for (label, file) in [
        ("deno.json", cfg.deno.as_ref()),
        ("package.json", cfg.pkg.as_ref()),
    ] {
        if named_set_in(file, "default")
            .map(|set| set_declares_grants(&set))
            .unwrap_or(false)
        {
            declaring.push(label);
        }
    }
    if !declaring.is_empty() {
        notes.push(format!(
            "{} declares permissions but none were selected for the build; the \
             artifact is deny-by-default (use compile.permissions, -P <set>, or \
             inka.permissions)",
            declaring.join(" and ")
        ));
    }
    (None, notes)
}

/// Synthesize manifest bytes from project config (no defaults applied yet).
/// Returns the lines; caller applies the runtime floor default + module= later.
pub struct Synth {
    pub bytes: Vec<u8>,
    pub warnings: Vec<String>,
}

/// Render the allow/deny permission DSL for an explicitly-selected named set
/// (`inka run -P [<name>]`). Only the named set is honored — never
/// `compile.permissions` or auto-defaults (dev-run intent). Returns the
/// newline-joined DSL lines (empty = deny-by-default) plus informational notes.
pub(crate) fn permission_set_dsl(cwd: &Path, name: &str) -> (String, Vec<String>) {
    let (cfg, load_warns) = load(cwd);
    let mut notes = load_warns;
    let Some(map) = resolve_named_set(&cfg, name, "-P", &mut notes) else {
        // resolve_named_set already recorded the unknown-name note.
        return (String::new(), notes);
    };
    let mut allow: Vec<(String, String)> = Vec::new();
    let mut deny: Vec<(String, String)> = Vec::new();
    apply_category_map(&map, &mut allow, &mut deny, &mut notes);
    validate_category_map(&map, &mut notes);
    warn_ineffective_denies(&allow, &deny, &mut notes);
    let mut lines: Vec<String> = Vec::new();
    for (cat, list) in allow {
        lines.push(format!("allow-{cat}={list}"));
    }
    for (cat, list) in deny {
        lines.push(format!("deny-{cat}={list}"));
    }
    (lines.join("\n"), notes)
}

/// Does the project config declare a non-empty `permissions.default` set that
/// would grant something? Used by `inka run` to hint when a dev-run has no
/// permission flags selected. Never applies the set.
pub(crate) fn config_has_default_grants(cwd: &Path) -> bool {
    let (cfg, _) = load(cwd);
    [cfg.deno.as_ref(), cfg.pkg.as_ref()]
        .iter()
        .any(|file| {
            named_set_in(*file, "default")
                .map(|set| set_declares_grants(&set))
                .unwrap_or(false)
        })
}

pub fn synthesize_manifest(cwd: &Path, perm_set: Option<&str>) -> Synth {
    let (cfg, load_warns) = load(cwd);
    let mut lines: Vec<String> = Vec::new();
    let mut warns: Vec<String> = load_warns;

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

    let (map, notes) = effective_permission_map(&cfg, perm_set);
    warns.extend(notes);
    if let Some(map) = map {
        let mut allow: Vec<(String, String)> = Vec::new();
        let mut deny: Vec<(String, String)> = Vec::new();
        apply_category_map(&map, &mut allow, &mut deny, &mut warns);
        validate_category_map(&map, &mut warns);
        warn_ineffective_denies(&allow, &deny, &mut warns);
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
        read_synth(cwd, perm_set).0
    }

    fn read_synth(cwd: &Path, perm_set: Option<&str>) -> (String, Vec<String>) {
        let s = synthesize_manifest(cwd, perm_set);
        (String::from_utf8(s.bytes).unwrap(), s.warnings)
    }

    fn no_permission_lines(s: &str) -> bool {
        !s.lines()
            .any(|l| l.starts_with("allow-") || l.starts_with("deny-"))
    }

    fn has_note(warns: &[String], needle: &str) -> bool {
        warns.iter().any(|w| w.contains(needle))
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
        let (s, warns) = read_synth(&cwd, None);
        assert_eq!(s, "");
        assert!(warns.is_empty(), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // A plain permissions.default is dev-run intent; without an explicit marker
    // it must NOT be baked, and an informational note is produced.
    #[test]
    fn plain_default_set_is_ignored_with_note() {
        let cwd = PathBuf::from("/tmp/inkaconf-default-only");
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
        let (s, warns) = read_synth(&cwd, None);
        assert!(no_permission_lines(&s), "deny-by-default expected: {s}");
        assert!(
            has_note(&warns, "deno.json declares permissions but none were selected"),
            "{warns:?}"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // Rich category shapes (bool/array/{allow,deny,ignore}/import) are honored
    // when the map is an explicit compile.permissions source.
    #[test]
    fn compile_permissions_map_applies_category_shapes() {
        let cwd = PathBuf::from("/tmp/inkaconf-compile-map");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{
  "permissions": { "default": { "run": true } },
  "compile": {
    "permissions": {
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
        let (s, warns) = read_synth(&cwd, None);
        assert!(s.contains("allow-read=./data,/etc"), "{s}");
        assert!(s.contains("allow-env=*"), "{s}");
        assert!(s.contains("allow-net=127.0.0.1"), "{s}");
        assert!(!s.contains("allow-write"), "{s}");
        assert!(!s.contains("allow-import"), "{s}");
        assert!(s.contains("deny-ffi=libc.so"), "{s}");
        assert!(has_note(&warns, "no inka equivalent"), "{warns:?}");
        assert!(has_note(&warns, "'ignore' has no inka equivalent"), "{warns:?}");
        // compile.permissions was selected, so the "none selected" note is absent.
        assert!(!has_note(&warns, "none were selected"), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // Deno allows scalar-string category values ("read": "./data"); they must
    // be baked, not silently dropped. A relative read path now also warns.
    #[test]
    fn scalar_string_permission_value_is_an_allow_entry() {
        let cwd = PathBuf::from("/tmp/inkaconf-scalar");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{
  "compile": {
    "permissions": {
      "read": "./data",
      "run": ["git", "curl"]
    }
  }
}"#,
        );
        let (s, warns) = read_synth(&cwd, None);
        assert!(s.contains("allow-read=./data"), "{s}");
        assert!(s.contains("allow-run=git,curl"), "{s}");
        assert!(has_note(&warns, "relative path"), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // Both files define the same set: with no marker it is ignored (+ note);
    // -P <name> selects it with deno.json winning per-category.
    #[test]
    fn named_set_selection_and_per_key_merge() {
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
        // No marker: defaults are ignored, runtime/tested-against still emit.
        let (s, warns) = read_synth(&cwd, None);
        assert!(no_permission_lines(&s), "deny-by-default expected: {s}");
        assert!(s.contains("runtime=inka_runtime>=0.266.0"), "{s}");
        assert!(s.contains("tested-against=0.266.0"), "{s}");
        assert!(has_note(&warns, "declares permissions but none were selected"), "{warns:?}");

        // -P default: both files define it -> per-category merge, deno wins.
        let s2 = read_manifest(&cwd, Some("default"));
        assert!(s2.contains("allow-read=./deno"), "deno read wins per key: {s2}");
        assert!(s2.contains("allow-net=*"), "{s2}");
        assert!(!s2.contains("./pkg"), "{s2}");

        // -P server (deno.json only) still works.
        let s3 = read_manifest(&cwd, Some("server"));
        assert!(s3.contains("allow-net=0.0.0.0:80"), "{s3}");
        assert!(!s3.contains("allow-read"), "{s3}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn compile_permissions_beat_default_and_runtime_exact() {
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
        let (s, warns) = read_synth(&cwd, None);
        assert!(s.contains("allow-env=*"), "compile perms win: {s}");
        assert!(!s.contains("allow-read"), "{s}");
        assert!(s.contains("runtime=inka_runtime==0.270.0"), "{s}");
        assert!(!has_note(&warns, "none were selected"), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn inka_permissions_marker_selects_set() {
        let cwd = PathBuf::from("/tmp/inkaconf-marker");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{
  "permissions": {
    "default": { "run": true },
    "server": { "net": ["0.0.0.0:80"] }
  },
  "inka": { "permissions": "server" }
}"#,
        );
        let (s, warns) = read_synth(&cwd, None);
        assert!(s.contains("allow-net=0.0.0.0:80"), "{s}");
        assert!(!s.contains("allow-run"), "{s}");
        assert!(!has_note(&warns, "none were selected"), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // A marker living in package.json may select a set defined in deno.json.
    #[test]
    fn marker_in_package_json_selects_deno_set() {
        let cwd = PathBuf::from("/tmp/inkaconf-marker-pkg");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "package.json",
            r#"{
  "name": "x",
  "inka": { "permissions": "server" }
}"#,
        );
        write(
            &cwd,
            "deno.json",
            r#"{
  "permissions": { "server": { "net": ["0.0.0.0:80"] } }
}"#,
        );
        let (s, warns) = read_synth(&cwd, None);
        assert!(s.contains("allow-net=0.0.0.0:80"), "{s}");
        assert!(warns.is_empty(), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // Unknown set name via compile.permissions -> warning + deny-by-default,
    // NOT a silent fall-back to the default set.
    #[test]
    fn unknown_compile_permissions_set_warns_and_denies_all() {
        let cwd = PathBuf::from("/tmp/inkaconf-badname");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{
  "permissions": { "default": { "run": true } },
  "compile": { "permissions": "nope" }
}"#,
        );
        let (s, warns) = read_synth(&cwd, None);
        assert!(no_permission_lines(&s), "deny-by-default expected: {s}");
        assert!(
            has_note(&warns, "compile.permissions names permission set 'nope'"),
            "{warns:?}"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // An explicit -P selecting the default set still bakes it (marker present).
    #[test]
    fn explicit_p_default_bakes_the_default_set() {
        let cwd = PathBuf::from("/tmp/inkaconf-p-default");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{
  "permissions": { "default": { "run": true, "net": true } }
}"#,
        );
        let (s, warns) = read_synth(&cwd, Some("default"));
        assert!(s.contains("allow-run=*"), "{s}");
        assert!(s.contains("allow-net=*"), "{s}");
        assert!(warns.is_empty(), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // Unknown -P name -> warning + deny-by-default.
    #[test]
    fn p_unknown_set_warns_and_denies_all() {
        let cwd = PathBuf::from("/tmp/inkaconf-p-nope");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{
  "permissions": { "default": { "run": true } }
}"#,
        );
        let (s, warns) = read_synth(&cwd, Some("nope"));
        assert!(no_permission_lines(&s), "deny-by-default expected: {s}");
        assert!(has_note(&warns, "-P names permission set 'nope'"), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // Malformed explicit sources warn and yield a deny-by-default artifact.
    #[test]
    fn malformed_compile_permissions_warns() {
        let cwd = PathBuf::from("/tmp/inkaconf-badcompile");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{ "compile": { "permissions": true } }"#,
        );
        let (s, warns) = read_synth(&cwd, None);
        assert!(no_permission_lines(&s), "deny-by-default expected: {s}");
        assert!(
            has_note(&warns, "compile.permissions must be a permissions map or a set-name string"),
            "{warns:?}"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn malformed_inka_marker_warns() {
        let cwd = PathBuf::from("/tmp/inkaconf-badmarker");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{ "permissions": { "default": { "run": true } }, "inka": { "permissions": true } }"#,
        );
        let (s, warns) = read_synth(&cwd, None);
        assert!(no_permission_lines(&s), "deny-by-default expected: {s}");
        assert!(
            has_note(&warns, "inka.permissions must name a permission set (string)"),
            "{warns:?}"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // An empty default set grants nothing, so it must not produce the note.
    #[test]
    fn empty_default_set_produces_no_note() {
        let cwd = PathBuf::from("/tmp/inkaconf-emptyd");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{ "permissions": { "default": {} } }"#,
        );
        let (s, warns) = read_synth(&cwd, None);
        assert_eq!(s, "");
        assert!(warns.is_empty(), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // A deny-only/allow-false default set grants nothing -> no note either.
    #[test]
    fn deny_only_default_set_produces_no_note() {
        let cwd = PathBuf::from("/tmp/inkaconf-denyonly");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{
  "permissions": {
    "default": {
      "write": false,
      "read": { "deny": ["/etc"] }
    }
  }
}"#,
        );
        let (s, warns) = read_synth(&cwd, None);
        assert_eq!(s, "");
        assert!(!has_note(&warns, "none were selected"), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // WS1-2: an existing deno.json with invalid JSON must produce a warning
    // naming the file (never silently treated as "no config"), while the build
    // still falls back to a deny-by-default manifest.
    #[test]
    fn malformed_deno_json_warns_and_yields_empty() {
        let cwd = PathBuf::from("/tmp/inkaconf-badjson");
        let _ = std::fs::remove_dir_all(&cwd);
        write(&cwd, "deno.json", r#"{ "permissions":"#);
        let (s, warns) = read_synth(&cwd, None);
        assert_eq!(s, "");
        assert!(has_note(&warns, "deno.json"), "{warns:?}");
        assert!(has_note(&warns, "not valid JSON"), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // A bad package.json warns but a valid deno.json is still honored.
    #[test]
    fn malformed_package_json_warns_but_deno_still_used() {
        let cwd = PathBuf::from("/tmp/inkaconf-badpkg");
        let _ = std::fs::remove_dir_all(&cwd);
        write(&cwd, "package.json", r#"{ "permissions": "#);
        write(
            &cwd,
            "deno.json",
            r#"{ "compile": { "permissions": { "env": true } } }"#,
        );
        let (s, warns) = read_synth(&cwd, None);
        assert!(s.contains("allow-env=*"), "{s}");
        assert!(has_note(&warns, "package.json"), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // A malformed deno.json falls through to a valid deno.jsonc, loudly.
    #[test]
    fn deno_jsonc_fallback_used_when_deno_json_malformed() {
        let cwd = PathBuf::from("/tmp/inkaconf-jsoncfallback");
        let _ = std::fs::remove_dir_all(&cwd);
        write(&cwd, "deno.json", r#"{ "compile": "#);
        write(
            &cwd,
            "deno.jsonc",
            r#"{
  // comment
  "compile": { "permissions": { "env": true }, },
}"#,
        );
        let (s, warns) = read_synth(&cwd, None);
        assert!(s.contains("allow-env=*"), "deno.jsonc used: {s}");
        assert!(has_note(&warns, "deno.json"), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // A config path that exists but cannot be read (here: a directory named
    // package.json) is a warning, not "no config".
    #[test]
    fn unreadable_config_warns() {
        let cwd = PathBuf::from("/tmp/inkaconf-unreadable");
        let _ = std::fs::remove_dir_all(&cwd);
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(cwd.join("package.json")).unwrap();
        let (s, warns) = read_synth(&cwd, None);
        assert_eq!(s, "");
        assert!(has_note(&warns, "package.json"), "{warns:?}");
        assert!(has_note(&warns, "cannot be read"), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // WS-`inka run`: the config hint helper only fires for a default set that
    // would actually grant something.
    #[test]
    fn config_has_default_grants_detects_nonempty_default() {
        let cwd = PathBuf::from("/tmp/inkaconf-rungrants");
        let _ = std::fs::remove_dir_all(&cwd);
        std::fs::create_dir_all(&cwd).unwrap();
        assert!(!config_has_default_grants(&cwd));
        write(&cwd, "deno.json", r#"{ "permissions": { "default": {} } }"#);
        assert!(!config_has_default_grants(&cwd), "empty default grants nothing");
        write(
            &cwd,
            "deno.json",
            r#"{ "permissions": { "default": { "run": true } } }"#,
        );
        assert!(config_has_default_grants(&cwd));
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn relative_read_path_warns() {
        let cwd = PathBuf::from("/tmp/inkaconf-relpath");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{ "compile": { "permissions": { "read": ["./data"] } } }"#,
        );
        let (s, warns) = read_synth(&cwd, None);
        assert!(s.contains("allow-read=./data"), "{s}");
        assert!(has_note(&warns, "relative path"), "{warns:?}");
        // an absolute path must not warn
        write(
            &cwd,
            "deno.json",
            r#"{ "compile": { "permissions": { "read": ["/etc"] } } }"#,
        );
        let (_, warns) = read_synth(&cwd, None);
        assert!(!has_note(&warns, "relative path"), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn deny_without_allow_warns() {
        let cwd = PathBuf::from("/tmp/inkaconf-denyonlywarn");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{ "compile": { "permissions": { "read": { "deny": ["/etc"] } } } }"#,
        );
        let (s, warns) = read_synth(&cwd, None);
        assert!(!s.contains("allow-read"), "{s}");
        assert!(has_note(&warns, "has no effect"), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn comma_in_permission_item_warns() {
        let cwd = PathBuf::from("/tmp/inkaconf-commaitem");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{ "compile": { "permissions": { "read": ["/a,b"] } } }"#,
        );
        let (_, warns) = read_synth(&cwd, None);
        assert!(has_note(&warns, "comma or newline"), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }
}


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
//   CLI flags  >  deno.json compile.permissions  >  inka.permissions marker.
// With no source selected the artifact is deny-by-default (no allow/deny
// lines), matching the manifest-less posture.
//
// Only the launcher-recognized manifest keys are emitted:
//   runtime=inka_runtime<spec>, tested-against=<v>,
//   allow-<cat>=<list> / deny-<cat>=<list>   (cat: read|write|net|env|run|sys|ffi)

use std::path::Path;

use serde_json::Value;

const CATEGORIES: [&str; 8] = ["read", "write", "net", "env", "run", "sys", "ffi", "import"];

/// Parse JSONC (JSON with comments and trailing commas) into a value.
///
/// Uses `jsonc-parser`, the same parser Deno uses, so a `deno.jsonc` (or any
/// config we choose to accept comments in) is read exactly as Deno reads it.
pub fn parse_jsonc(input: &str) -> Result<Value, String> {
    jsonc_parser::parse_to_serde_value(input, &Default::default()).map_err(|e| e.to_string())
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
    // `.jsonc` configs accept comments/trailing commas; plain `.json` stays
    // strict. A parse failure is an Unparseable case, not a read error.
    let parsed = if name.ends_with(".jsonc") {
        parse_jsonc(&raw)
    } else {
        serde_json::from_str(&raw).map_err(|e| e.to_string())
    };
    match parsed {
        Ok(v) => LoadOutcome::Ok(v),
        Err(e) => LoadOutcome::Unparseable(format!("{} is not valid JSON: {e}", path.display())),
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
    if let Some(blk) = cfg.deno.as_ref().and_then(|d| d.get("inka")) {
        if let Some(r) = blk.get("runtime").and_then(Value::as_str) {
            runtime = Some(r.to_string());
        }
        if let Some(t) = blk.get("tested-against").and_then(Value::as_str) {
            tested = Some(t.to_string());
        }
    }
    (runtime, tested)
}

/// The effective `inka.path-base` (`exe` or `cwd`); deno.json wins.
fn inka_block_path_base(cfg: &ConfigFiles) -> Option<String> {
    let read = |f: Option<&Value>| {
        f.and_then(|v| v.get("inka"))
            .and_then(|i| i.get("path-base"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    read(cfg.deno.as_ref()).or_else(|| read(cfg.pkg.as_ref()))
}

/// True when a rendered comma-list carries at least one non-empty item.
/// Guards against a value like `""`, `" "`, or `","` being mistaken for a grant
/// (the runtime treats an empty list as "all", which would be an over-grant).
fn list_has_items(list: &str) -> bool {
    list.split(',').any(|s| !s.trim().is_empty())
}

/// Render an allow/deny value: `true` => "*", array => comma list. Empty or
/// whitespace-only array items are dropped so the result never has empty slots.
fn render_val(v: &Value) -> Option<String> {
    match v {
        Value::Bool(true) => Some("*".to_string()),
        Value::Bool(false) => None,
        Value::String(s) => {
            if s.trim().is_empty() {
                None
            } else {
                Some(s.clone())
            }
        }
        Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .filter_map(|i| i.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
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

/// Push a rendered allow/deny list. A raw newline is a hard error (it would
/// inject an extra manifest line); an itemless list warns and is skipped (an
/// empty list would become "all" in the runtime DSL — an over-grant).
fn push_permission(
    target: &mut Vec<(String, String)>,
    kind: &str,
    cat: &str,
    list: String,
    warns: &mut Vec<String>,
) -> Result<(), String> {
    if list.contains('\n') || list.contains('\r') {
        return Err(format!(
            "permission '{cat}' {kind} list contains a newline, which the manifest cannot represent"
        ));
    }
    if list_has_items(&list) {
        target.push((cat.to_string(), list));
    } else {
        warns.push(format!(
            "permission '{cat}' {kind} list is empty; ignored (use \"*\" for all)"
        ));
    }
    Ok(())
}

/// Apply one category map (`{ cat: bool | string | array | {allow,deny,ignore} }`)
/// into aggregated allow/deny lists. deno's `ignore` sub-key has no inka
/// equivalent.
fn apply_category_map(
    map: &Value,
    allow: &mut Vec<(String, String)>,
    deny: &mut Vec<(String, String)>,
    warns: &mut Vec<String>,
) -> Result<(), String> {
    let Some(obj) = map.as_object() else {
        return Ok(());
    };
    for (cat, val) in obj {
        if !CATEGORIES.contains(&cat.as_str()) {
            warns.push(format!("unknown permission category '{cat}'; ignored"));
            continue;
        }
        match val {
            Value::Bool(b) => {
                if *b {
                    allow.push((cat.clone(), "*".to_string()));
                }
            }
            Value::Array(_) => match render_val(val) {
                Some(list) => push_permission(allow, "allow", cat, list, warns)?,
                None => push_permission(allow, "allow", cat, String::new(), warns)?,
            },
            Value::String(s) => push_permission(allow, "allow", cat, s.clone(), warns)?,
            Value::Object(o) => {
                if let Some(a) = o.get("allow") {
                    push_permission(
                        allow,
                        "allow",
                        cat,
                        render_val(a).unwrap_or_default(),
                        warns,
                    )?;
                }
                if let Some(d) = o.get("deny") {
                    push_permission(deny, "deny", cat, render_val(d).unwrap_or_default(), warns)?;
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
    Ok(())
}

/// True when a permission descriptor looks like a relative filesystem path
/// (not `*`, not absolute, not a URL/scheme, and not a portable token like
/// `${EXE_DIR}` whose base is supplied at run time).
fn is_relative_path(item: &str) -> bool {
    !item.is_empty()
        && item != "*"
        && !item.starts_with('/')
        && !item.contains("://")
        && !item.contains("${")
}

/// Accept the runtime-requirement grammar the launcher understands: an optional
/// `>=`/`>`/`==` prefix followed by a version (`0.266.2`, optionally suffixed
/// `-beta.N`/`-rc.N`). Rejects empty and injected newlines.
pub(crate) fn valid_version_spec(spec: &str) -> bool {
    if spec.contains('\n') || spec.contains('\r') {
        return false;
    }
    let rest = spec
        .strip_prefix(">=")
        .or_else(|| spec.strip_prefix("=="))
        .or_else(|| spec.strip_prefix('>'))
        .unwrap_or(spec);
    !rest.is_empty() && crate::parse_version(rest).is_some()
}

/// Turn a runtime spec (`>=0.266.7`, `==0.266.7`, or a bare `0.266.7`) into the
/// `inka_runtime…` value used in a `runtime=` manifest line.
pub(crate) fn runtime_value(spec: &str) -> String {
    let spec = spec.trim();
    if spec.starts_with('>') || spec.starts_with('=') {
        format!("inka_runtime{spec}")
    } else {
        format!("inka_runtime=={spec}")
    }
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
            // A non-object deno.json set is authoritative: it grants nothing and
            // must not silently fall back to the package.json definition.
            let Some(dobj) = dv.as_object() else {
                return Some(dv);
            };
            let mut merged = if pv.is_object() {
                pv
            } else {
                Value::Object(Default::default())
            };
            if let Some(mobj) = merged.as_object_mut() {
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
        Value::String(s) => list_has_items(s),
        Value::Array(_) => render_val(v).is_some_and(|l| list_has_items(&l)),
        Value::Object(o) => o
            .get("allow")
            .and_then(render_val)
            .is_some_and(|l| list_has_items(&l)),
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

/// A permission source selected for baking.
pub(crate) enum PermSelection {
    /// `permissions=all` (from `inka.permissions = "all"`).
    All,
    /// A category map (`{ "env": true, "read": ["./"] }`).
    Map(Value),
}

/// Pick the permission source to bake for the build, plus informational notes.
/// Only explicit build-intent sources are ever baked, in this order:
///   1. CLI `-P/--permission-set <name>` — a named set (or a CLI grant DSL,
///      handled by the caller and passed as an override).
///   2. deno.json `compile.permissions` — the deno-compile analog; either a
///      direct category map, or a string naming a set. `inka build` *is* the
///      compile step, so this bakes automatically (documented divergence from
///      Deno, which requires `-P` even for compile permissions).
///   3. An `inka.permissions` marker (deno.json wins over package.json): the
///      string `"all"` (`permissions=all`), a set-name string, or a category
///      map object. This is the manager-agnostic path for projects without a
///      deno.json (e.g. npm/pnpm/yarn/bun `package.json`).
///
/// A plain `permissions.default` set with none of the above markers is dev-run
/// intent and is IGNORED; if such a set would actually grant something, an
/// informational note is returned so the silent drop is never invisible. With
/// no source at all, an advisory note points at the ways to grant access.
/// Unknown or malformed explicit sources warn and yield a deny-by-default
/// artifact (no `allow-*`/`deny-*` lines).
fn effective_permission_map(
    cfg: &ConfigFiles,
    perm_set: Option<&str>,
) -> (Option<PermSelection>, Vec<String>) {
    let mut notes: Vec<String> = Vec::new();

    // 1. explicit CLI set selection
    if let Some(name) = perm_set {
        return (
            resolve_named_set(cfg, name, "-P", &mut notes).map(PermSelection::Map),
            notes,
        );
    }

    // 2. deno.json compile.permissions (deno-compile analog)
    if let Some(compile) = cfg.deno.as_ref().and_then(|d| d.get("compile")) {
        if let Some(p) = compile.get("permissions") {
            if p.is_object() {
                return (Some(PermSelection::Map(p.clone())), notes);
            }
            if let Some(name) = p.as_str() {
                return (
                    resolve_named_set(cfg, name, "compile.permissions", &mut notes)
                        .map(PermSelection::Map),
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
    if let Some(v) = deno_marker.or(pkg_marker) {
        return match v {
            Value::String(s) if s == "all" => (Some(PermSelection::All), notes),
            Value::String(name) => (
                resolve_named_set(cfg, name, "inka.permissions", &mut notes)
                    .map(PermSelection::Map),
                notes,
            ),
            Value::Object(_) => (Some(PermSelection::Map(v.clone())), notes),
            _ => {
                notes.push(
                    "inka.permissions must be \"all\", a set-name string, or a \
                     permission map; the artifact is deny-by-default"
                        .to_string(),
                );
                (None, notes)
            }
        };
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
             artifact is deny-by-default (use compile.permissions, -P=<set>, or \
             inka.permissions)",
            declaring.join(" and ")
        ));
    } else {
        notes.push(
            "no permission source found; the artifact is deny-by-default (use \
             --allow-*/-A, -P=<set>, or set inka.permissions in package.json)"
                .to_string(),
        );
    }
    (None, notes)
}

/// Synthesized manifest bytes plus any non-fatal warnings.
#[derive(Debug)]
pub struct Synth {
    pub bytes: Vec<u8>,
    pub warnings: Vec<String>,
}

/// Render the allow/deny permission DSL for an explicitly-selected named set
/// (`inka run -P [<name>]`). Only the named set is honored — never
/// `compile.permissions` or auto-defaults (dev-run intent). Returns the
/// newline-joined DSL lines (empty = deny-by-default) plus informational notes.
pub(crate) fn permission_set_dsl(cwd: &Path, name: &str) -> Result<(String, Vec<String>), String> {
    let (cfg, load_warns) = load(cwd);
    let mut notes = load_warns;
    let Some(map) = resolve_named_set(&cfg, name, "-P", &mut notes) else {
        // resolve_named_set already recorded the unknown-name note.
        return Ok((String::new(), notes));
    };
    let mut allow: Vec<(String, String)> = Vec::new();
    let mut deny: Vec<(String, String)> = Vec::new();
    apply_category_map(&map, &mut allow, &mut deny, &mut notes)?;
    validate_category_map(&map, &mut notes);
    warn_ineffective_denies(&allow, &deny, &mut notes);
    let mut lines: Vec<String> = Vec::new();
    for (cat, list) in allow {
        lines.push(format!("allow-{cat}={list}"));
    }
    for (cat, list) in deny {
        lines.push(format!("deny-{cat}={list}"));
    }
    Ok((lines.join("\n"), notes))
}

/// Does the project config declare a non-empty `permissions.default` set that
/// would grant something? Used by `inka run` to hint when a dev-run has no
/// permission flags selected. Never applies the set.
pub(crate) fn config_has_default_grants(cwd: &Path) -> bool {
    let (cfg, _) = load(cwd);
    [cfg.deno.as_ref(), cfg.pkg.as_ref()].iter().any(|file| {
        named_set_in(*file, "default")
            .map(|set| set_declares_grants(&set))
            .unwrap_or(false)
    })
}

/// Resolved `desktop` block for `inka desktop`. Deno-parity fields are read
/// from `deno.json`/`deno.jsonc` (top-level `desktop`); `package.json`'s
/// `inka.desktop` is an inka-specific fallback. Deno config wins per-key.
///
/// Only the fields Linux packaging honors today are surfaced. Malformed fields
/// produce a warning and are ignored rather than failing the build.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct DesktopConfig {
    /// Directory the config was discovered in. Relative `output`/`icon` paths
    /// resolve against this, not the process CWD (matching how Deno treats
    /// config-relative paths).
    pub base_dir: std::path::PathBuf,
    pub app_name: Option<String>,
    pub identifier: Option<String>,
    /// Linux icon (path(s) relative to `base_dir`).
    pub icon_linux: Option<DesktopIcon>,
    /// Windows icon (path(s) relative to `base_dir`).
    pub icon_windows: Option<DesktopIcon>,
    pub backend: Option<String>,
    pub output_linux: Option<String>,
    /// Windows output directory (relative to `base_dir`).
    pub output_windows: Option<String>,
    pub release_base: Option<String>,
    pub error_reporting: Option<String>,
    /// Top-level `version`, used as the auto-update app version default.
    pub version: Option<String>,
}

/// Walk up from `start` to the nearest directory containing a project config
/// (`deno.json`/`deno.jsonc`/`package.json`). Returns `start` when none is
/// found. `inka desktop` is config-discovering (like `deno desktop`); other
/// commands stay CWD-scoped.
pub(crate) fn discover_config_dir(start: &Path) -> std::path::PathBuf {
    let mut dir = start;
    loop {
        for name in ["deno.json", "deno.jsonc", "package.json"] {
            if dir.join(name).is_file() {
                return dir.to_path_buf();
            }
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return start.to_path_buf(),
        }
    }
}

/// Read a non-empty string field, warning when it has the wrong shape.
fn desktop_string(
    obj: Option<&Value>,
    key: &str,
    label: &str,
    warns: &mut Vec<String>,
) -> Option<String> {
    match obj.and_then(|o| o.get(key)) {
        None => None,
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(_) => {
            warns.push(format!("{label} must be a non-empty string; ignoring it"));
            None
        }
    }
}

/// A platform icon config: a single path, or Deno's list of `{ path, size }`
/// entries. A set is preserved so Windows can build a multi-resolution `.ico`;
/// Linux ships the largest entry.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DesktopIcon {
    Single(String),
    Set(Vec<(String, u32)>),
}

/// Resolve a platform icon value: a single path string, or a list of
/// `{ path, size }` entries.
fn desktop_icon(
    value: Option<&Value>,
    label: &str,
    warns: &mut Vec<String>,
) -> Option<DesktopIcon> {
    match value {
        None => None,
        Some(Value::String(s)) if !s.is_empty() => Some(DesktopIcon::Single(s.clone())),
        Some(Value::Array(entries)) => {
            let mut set: Vec<(String, u32)> = Vec::new();
            for entry in entries {
                let path = entry.get("path").and_then(Value::as_str);
                let size = entry.get("size").and_then(Value::as_u64).unwrap_or(0);
                if let Some(path) = path.filter(|p| !p.is_empty()) {
                    set.push((path.to_string(), size.min(u32::MAX as u64) as u32));
                }
            }
            if set.is_empty() {
                warns.push(format!(
                    "{label} entries need a non-empty `path`; ignoring them"
                ));
                None
            } else {
                Some(DesktopIcon::Set(set))
            }
        }
        Some(_) => {
            warns.push(format!(
                "{label} must be a path string or a [{{ path, size }}] array; ignoring it"
            ));
            None
        }
    }
}

/// The `desktop` config object: `deno.json`'s top-level `desktop` when present,
/// else `package.json`'s `inka.desktop`.
fn desktop_block(cfg: &ConfigFiles) -> Option<&Value> {
    cfg.deno
        .as_ref()
        .and_then(|d| d.get("desktop"))
        .or_else(|| {
            cfg.pkg
                .as_ref()
                .and_then(|p| p.get("inka"))
                .and_then(|i| i.get("desktop"))
        })
}

/// Resolve the `desktop` config plus non-fatal warnings. `version` comes from
/// the top-level `version` and seeds `--app-version`. The config is discovered
/// by walking up from `cwd` (like `deno desktop`), so running from a subdir
/// still finds the project's `deno.json`.
pub(crate) fn desktop_config(cwd: &Path) -> (DesktopConfig, Vec<String>) {
    let base = discover_config_dir(cwd);
    let (cfg, mut warns) = load(&base);
    let mut out = DesktopConfig {
        base_dir: base,
        version: cfg
            .deno
            .as_ref()
            .and_then(|d| d.get("version"))
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string),
        ..Default::default()
    };

    let Some(block) = desktop_block(&cfg) else {
        return (out, warns);
    };
    if !block.is_object() {
        warns.push("`desktop` must be an object; ignoring it".to_string());
        return (out, warns);
    }

    let app = block.get("app");
    out.app_name = desktop_string(app, "name", "desktop.app.name", &mut warns);
    out.identifier = desktop_string(app, "identifier", "desktop.app.identifier", &mut warns);
    let icons = app.and_then(|a| a.get("icons"));
    out.icon_linux = desktop_icon(
        icons.and_then(|i| i.get("linux")),
        "desktop.app.icons.linux",
        &mut warns,
    );
    out.icon_windows = desktop_icon(
        icons.and_then(|i| i.get("windows")),
        "desktop.app.icons.windows",
        &mut warns,
    );
    out.backend = desktop_string(Some(block), "backend", "desktop.backend", &mut warns);
    let output = block.get("output");
    out.output_linux = desktop_string(output, "linux", "desktop.output.linux", &mut warns);
    out.output_windows = desktop_string(output, "windows", "desktop.output.windows", &mut warns);
    out.release_base = desktop_string(
        block.get("release"),
        "baseUrl",
        "desktop.release.baseUrl",
        &mut warns,
    );
    out.error_reporting = desktop_string(
        block.get("errorReporting"),
        "url",
        "desktop.errorReporting.url",
        &mut warns,
    );

    // macOS packaging is unimplemented, so flag macos-only keys rather than
    // silently ignoring them. linux/windows keys are host-selected and normal
    // to set side by side, so neither is warned about.
    if out.output_linux.is_none()
        && out.output_windows.is_none()
        && output.and_then(|o| o.get("macos")).is_some()
    {
        warns.push(
            "desktop.output.macos is ignored (macOS packaging is not implemented); \
             set desktop.output.linux or desktop.output.windows"
                .to_string(),
        );
    }
    if out.icon_linux.is_none()
        && out.icon_windows.is_none()
        && icons.and_then(|i| i.get("macos")).is_some()
    {
        warns.push(
            "desktop.app.icons.macos is ignored (macOS packaging is not implemented); \
             set desktop.app.icons.linux or desktop.app.icons.windows"
                .to_string(),
        );
    }

    (out, warns)
}

/// A build-intent permission source that `inka build` bakes automatically but
/// `inka run` does not apply (a documented asymmetry: run is deny-by-default
/// unless flags or `-P` are given).
pub(crate) struct BuildIntentHint {
    /// Human-readable source, e.g. `deno.json compile.permissions`.
    pub source: String,
    /// The named set to select with `-P=<name>`, when the source names one.
    pub set_name: Option<String>,
}

/// Detect an explicit build-intent permission source (`compile.permissions` or
/// an `inka.permissions` marker) so `inka run` can point the user at the flags
/// that reproduce what a build would bake. Returns `None` when there is none.
pub(crate) fn build_intent_permission_hint(cwd: &Path) -> Option<BuildIntentHint> {
    let (cfg, _) = load(cwd);

    if let Some(p) = cfg
        .deno
        .as_ref()
        .and_then(|d| d.get("compile"))
        .and_then(|c| c.get("permissions"))
    {
        if p.is_object() {
            return Some(BuildIntentHint {
                source: "deno.json compile.permissions (category map)".to_string(),
                set_name: None,
            });
        }
        if let Some(name) = p.as_str() {
            return Some(BuildIntentHint {
                source: format!("deno.json compile.permissions set '{name}'"),
                set_name: Some(name.to_string()),
            });
        }
        return Some(BuildIntentHint {
            source: "deno.json compile.permissions (malformed)".to_string(),
            set_name: None,
        });
    }

    let marker = cfg
        .deno
        .as_ref()
        .and_then(|d| d.get("inka"))
        .and_then(|i| i.get("permissions"))
        .or_else(|| {
            cfg.pkg
                .as_ref()
                .and_then(|p| p.get("inka"))
                .and_then(|i| i.get("permissions"))
        });
    match marker {
        Some(serde_json::Value::String(name)) if name == "all" => Some(BuildIntentHint {
            source: "inka.permissions (all)".to_string(),
            set_name: None,
        }),
        Some(serde_json::Value::String(name)) => Some(BuildIntentHint {
            source: format!("inka.permissions set '{name}'"),
            set_name: Some(name.clone()),
        }),
        Some(serde_json::Value::Object(_)) => Some(BuildIntentHint {
            source: "inka.permissions (category map)".to_string(),
            set_name: None,
        }),
        Some(_) => Some(BuildIntentHint {
            source: "inka.permissions (malformed)".to_string(),
            set_name: None,
        }),
        None => None,
    }
}

/// Synthesize manifest bytes from project config (no defaults applied yet).
/// `cli_dsl`, when present, is a permission DSL rendered from `inka build`'s
/// CLI flags; it overrides the config permission source entirely. Caller
/// applies the runtime floor default + module= later.
pub fn synthesize_manifest(
    cwd: &Path,
    perm_set: Option<&str>,
    cli_dsl: Option<&str>,
) -> Result<Synth, String> {
    let (cfg, load_warns) = load(cwd);
    let mut lines: Vec<String> = Vec::new();
    let mut warns: Vec<String> = load_warns;

    let (runtime, tested) = inka_block_runtime(&cfg);
    if let Some(r) = runtime {
        if !valid_version_spec(&r) {
            return Err(format!(
                "inka.runtime must be a version like 0.266.2 (optionally >=/==), got '{r}'"
            ));
        }
        lines.push(format!("runtime={}", runtime_value(&r)));
    }
    if let Some(t) = tested {
        if !valid_version_spec(&t) {
            return Err(format!(
                "inka.tested-against must be a version like 0.266.2, got '{t}'"
            ));
        }
        lines.push(format!("tested-against={t}"));
    }
    if let Some(pb) = inka_block_path_base(&cfg) {
        if pb != "exe" && pb != "cwd" {
            return Err(format!(
                "inka.path-base must be \"exe\" or \"cwd\", got '{pb}'"
            ));
        }
        lines.push(format!("path-base={pb}"));
    }

    if let Some(dsl) = cli_dsl {
        for line in dsl.lines() {
            if !line.trim().is_empty() {
                lines.push(line.to_string());
            }
        }
    } else {
        let (sel, notes) = effective_permission_map(&cfg, perm_set);
        warns.extend(notes);
        match sel {
            Some(PermSelection::All) => lines.push("permissions=all".to_string()),
            Some(PermSelection::Map(map)) => {
                let mut allow: Vec<(String, String)> = Vec::new();
                let mut deny: Vec<(String, String)> = Vec::new();
                apply_category_map(&map, &mut allow, &mut deny, &mut warns)?;
                validate_category_map(&map, &mut warns);
                warn_ineffective_denies(&allow, &deny, &mut warns);
                for (cat, list) in allow {
                    lines.push(format!("allow-{cat}={list}"));
                }
                for (cat, list) in deny {
                    lines.push(format!("deny-{cat}={list}"));
                }
            }
            None => {}
        }
    }

    let mut bytes = String::new();
    if !lines.is_empty() {
        bytes.push_str(&lines.join("\n"));
        bytes.push('\n');
    }
    Ok(Synth {
        bytes: bytes.into_bytes(),
        warnings: warns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A per-process-unique scratch dir under the system temp dir, emptied
    /// first so tests cannot collide across concurrent runs.
    fn scratch_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("inkaconf-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(cwd: &Path, name: &str, content: &str) {
        std::fs::create_dir_all(cwd).unwrap();
        std::fs::write(cwd.join(name), content).unwrap();
    }

    fn read_manifest(cwd: &Path, perm_set: Option<&str>) -> String {
        read_synth(cwd, perm_set).0
    }

    fn read_synth(cwd: &Path, perm_set: Option<&str>) -> (String, Vec<String>) {
        let s = synthesize_manifest(cwd, perm_set, None).unwrap();
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
    fn jsonc_parses_comments_and_trailing_commas() {
        let v = parse_jsonc(
            "{\n  // a comment\n  \"imports\": {\"a\": \"b\",}, /* block */ \"n\": 1,}",
        )
        .expect("jsonc");
        assert_eq!(v["n"], serde_json::json!(1));
        assert_eq!(v["imports"]["a"], serde_json::json!("b"));
    }

    #[test]
    fn jsonc_trailing_comma_before_comment() {
        // A trailing comma followed by a comment before the closing bracket.
        let v =
            parse_jsonc("{\n  \"a\": [1, // note\n  ],\n  \"b\": 2, /* x */\n}").expect("jsonc");
        assert_eq!(v["a"], serde_json::json!([1]));
    }

    #[test]
    fn empty_config_emits_nothing_but_advisory() {
        let cwd = scratch_dir("empty");
        let _ = std::fs::remove_dir_all(&cwd);
        std::fs::create_dir_all(&cwd).unwrap();
        let (s, warns) = read_synth(&cwd, None);
        assert_eq!(s, "");
        assert!(has_note(&warns, "no permission source found"), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // A plain permissions.default is dev-run intent; without an explicit marker
    // it must NOT be baked, and an informational note is produced.
    #[test]
    fn plain_default_set_is_ignored_with_note() {
        let cwd = scratch_dir("default-only");
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
            has_note(
                &warns,
                "deno.json declares permissions but none were selected"
            ),
            "{warns:?}"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // Rich category shapes (bool/array/{allow,deny,ignore}/import) are honored
    // when the map is an explicit compile.permissions source.
    #[test]
    fn compile_permissions_map_applies_category_shapes() {
        let cwd = scratch_dir("compile-map");
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
        assert!(s.contains("allow-import=*"), "{s}");
        assert!(s.contains("deny-ffi=libc.so"), "{s}");
        assert!(
            has_note(&warns, "'ignore' has no inka equivalent"),
            "{warns:?}"
        );
        // compile.permissions was selected, so the "none selected" note is absent.
        assert!(!has_note(&warns, "none were selected"), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // Deno allows scalar-string category values ("read": "./data"); they must
    // be baked, not silently dropped. A relative read path now also warns.
    #[test]
    fn scalar_string_permission_value_is_an_allow_entry() {
        let cwd = scratch_dir("scalar");
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
    // -P=<name> selects it with deno.json winning per-category.
    #[test]
    fn named_set_selection_and_per_key_merge() {
        let cwd = scratch_dir("both");
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
        assert!(
            has_note(&warns, "declares permissions but none were selected"),
            "{warns:?}"
        );

        // -P default: both files define it -> per-category merge, deno wins.
        let s2 = read_manifest(&cwd, Some("default"));
        assert!(
            s2.contains("allow-read=./deno"),
            "deno read wins per key: {s2}"
        );
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
        let cwd = scratch_dir("compile");
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
        let cwd = scratch_dir("marker");
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
        let cwd = scratch_dir("marker-pkg");
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
        let cwd = scratch_dir("badname");
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
        let cwd = scratch_dir("p-default");
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
        let cwd = scratch_dir("p-nope");
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
        assert!(
            has_note(&warns, "-P names permission set 'nope'"),
            "{warns:?}"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // Malformed explicit sources warn and yield a deny-by-default artifact.
    #[test]
    fn malformed_compile_permissions_warns() {
        let cwd = scratch_dir("badcompile");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{ "compile": { "permissions": true } }"#,
        );
        let (s, warns) = read_synth(&cwd, None);
        assert!(no_permission_lines(&s), "deny-by-default expected: {s}");
        assert!(
            has_note(
                &warns,
                "compile.permissions must be a permissions map or a set-name string"
            ),
            "{warns:?}"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn malformed_inka_marker_warns() {
        let cwd = scratch_dir("badmarker");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{ "permissions": { "default": { "run": true } }, "inka": { "permissions": true } }"#,
        );
        let (s, warns) = read_synth(&cwd, None);
        assert!(no_permission_lines(&s), "deny-by-default expected: {s}");
        assert!(has_note(&warns, "inka.permissions must be"), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // An empty default set grants nothing, so it must not produce the drop note
    // (the no-source advisory is still emitted).
    #[test]
    fn empty_default_set_produces_no_drop_note() {
        let cwd = scratch_dir("emptyd");
        let _ = std::fs::remove_dir_all(&cwd);
        write(&cwd, "deno.json", r#"{ "permissions": { "default": {} } }"#);
        let (s, warns) = read_synth(&cwd, None);
        assert_eq!(s, "");
        assert!(!has_note(&warns, "none were selected"), "{warns:?}");
        assert!(has_note(&warns, "no permission source found"), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // A deny-only/allow-false default set grants nothing -> no note either.
    #[test]
    fn deny_only_default_set_produces_no_note() {
        let cwd = scratch_dir("denyonly");
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
        let cwd = scratch_dir("badjson");
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
        let cwd = scratch_dir("badpkg");
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
        let cwd = scratch_dir("jsoncfallback");
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
        let cwd = scratch_dir("unreadable");
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
        let cwd = scratch_dir("rungrants");
        let _ = std::fs::remove_dir_all(&cwd);
        std::fs::create_dir_all(&cwd).unwrap();
        assert!(!config_has_default_grants(&cwd));
        write(&cwd, "deno.json", r#"{ "permissions": { "default": {} } }"#);
        assert!(
            !config_has_default_grants(&cwd),
            "empty default grants nothing"
        );
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
        let cwd = scratch_dir("relpath");
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
    fn token_path_does_not_warn_relative() {
        let cwd = scratch_dir("tokenrel");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{ "compile": { "permissions": { "read": ["${EXE_DIR}/data"] } } }"#,
        );
        let (s, warns) = read_synth(&cwd, None);
        assert!(s.contains("allow-read=${EXE_DIR}/data"), "{s}");
        assert!(!has_note(&warns, "relative path"), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn inka_path_base_is_emitted_and_validated() {
        let cwd = scratch_dir("pathbase");
        let _ = std::fs::remove_dir_all(&cwd);
        write(&cwd, "deno.json", r#"{ "inka": { "path-base": "exe" } }"#);
        let (s, _) = read_synth(&cwd, None);
        assert!(s.contains("path-base=exe"), "{s}");
        write(&cwd, "deno.json", r#"{ "inka": { "path-base": "bogus" } }"#);
        assert!(synthesize_manifest(&cwd, None, None).is_err());
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn deny_without_allow_warns() {
        let cwd = scratch_dir("denyonlywarn");
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
        let cwd = scratch_dir("commaitem");
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

    // An empty allow list must never bake `allow-<cat>=` (the runtime treats an
    // empty list as "all" — an over-grant, incl. run/ffi).
    #[test]
    fn empty_allow_list_is_ignored_with_warning() {
        let cwd = scratch_dir("emptyallow");
        let _ = std::fs::remove_dir_all(&cwd);

        for body in [
            r#"{ "compile": { "permissions": { "run": [""] } } }"#,
            r#"{ "compile": { "permissions": { "run": [","] } } }"#,
            r#"{ "compile": { "permissions": { "run": " " } } }"#,
            r#"{ "compile": { "permissions": { "run": { "allow": [""] } } } }"#,
        ] {
            write(&cwd, "deno.json", body);
            let (s, warns) = read_synth(&cwd, None);
            assert!(!s.contains("allow-run"), "must not bake empty allow: {s}");
            assert!(has_note(&warns, "allow list is empty"), "{body}: {warns:?}");
        }

        // A real item mixed with an empty one keeps the real item only.
        write(
            &cwd,
            "deno.json",
            r#"{ "compile": { "permissions": { "read": ["/data", ""] } } }"#,
        );
        let (s, _) = read_synth(&cwd, None);
        assert!(s.contains("allow-read=/data"), "{s}");
        assert!(!s.contains("allow-read=/data,"), "{s}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // A raw newline in a permission item would inject a manifest line.
    #[test]
    fn newline_in_permission_item_is_rejected() {
        let cwd = scratch_dir("injectitem");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{ "compile": { "permissions": { "read": ["/a\npermissions=all"] } } }"#,
        );
        let err = synthesize_manifest(&cwd, None, None)
            .expect_err("newline must be rejected")
            .to_string();
        assert!(err.contains("newline"), "{err}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn newline_in_runtime_is_rejected() {
        let cwd = scratch_dir("injectrt");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{ "inka": { "runtime": ">=0.266.2\npermissions=all" } }"#,
        );
        assert!(synthesize_manifest(&cwd, None, None).is_err());
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn malformed_runtime_spec_is_rejected() {
        let cwd = scratch_dir("badrt");
        let _ = std::fs::remove_dir_all(&cwd);
        write(&cwd, "deno.json", r#"{ "inka": { "runtime": ">=abc" } }"#);
        let err = synthesize_manifest(&cwd, None, None)
            .expect_err("bad runtime spec must be rejected")
            .to_string();
        assert!(err.contains("inka.runtime"), "{err}");
        // A well-formed spec still works.
        write(
            &cwd,
            "deno.json",
            r#"{ "inka": { "runtime": ">=0.266.2" } }"#,
        );
        let (s, _) = read_synth(&cwd, None);
        assert!(s.contains("runtime=inka_runtime>=0.266.2"), "{s}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // A non-object deno.json set is authoritative; package.json must not win.
    #[test]
    fn deno_non_object_set_is_authoritative() {
        let cwd = scratch_dir("nonobjset");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{ "permissions": { "server": true } }"#,
        );
        write(
            &cwd,
            "package.json",
            r#"{ "permissions": { "server": { "net": true } } }"#,
        );
        let (s, _) = read_synth(&cwd, Some("server"));
        assert!(!s.contains("allow-net"), "deno.json must win: {s}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // WS-`inka run` parity: the hint reports the build-intent source and, when
    // it names a set, the `-P=<name>` that reproduces a build's permissions.
    #[test]
    fn build_intent_hint_reports_named_and_map_sources() {
        let cwd = scratch_dir("buildintent");
        let _ = std::fs::remove_dir_all(&cwd);
        std::fs::create_dir_all(&cwd).unwrap();

        assert!(build_intent_permission_hint(&cwd).is_none());

        // compile.permissions naming a set -> selectable with -P=<name>.
        write(
            &cwd,
            "deno.json",
            r#"{ "compile": { "permissions": "server" }, "permissions": { "server": { "net": true } } }"#,
        );
        let h = build_intent_permission_hint(&cwd).expect("hint");
        assert_eq!(h.set_name.as_deref(), Some("server"), "{}", h.source);

        // compile.permissions as a category map -> no -P target.
        write(
            &cwd,
            "deno.json",
            r#"{ "compile": { "permissions": { "read": ["./data"] } } }"#,
        );
        let h = build_intent_permission_hint(&cwd).expect("hint");
        assert!(h.set_name.is_none(), "{}", h.source);
        assert!(h.source.contains("category map"), "{}", h.source);

        // inka.permissions marker -> selectable with -P=<name>.
        write(
            &cwd,
            "deno.json",
            r#"{ "inka": { "permissions": "server" }, "permissions": { "server": { "net": true } } }"#,
        );
        let h = build_intent_permission_hint(&cwd).expect("hint");
        assert_eq!(h.set_name.as_deref(), Some("server"), "{}", h.source);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // Non-Deno accommodation: a package.json-only project can declare
    // build-intent permissions inline via the `inka.permissions` marker.
    #[test]
    fn package_json_inline_permission_map_is_baked() {
        let cwd = scratch_dir("pkgmap");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "package.json",
            r#"{ "name": "t", "inka": { "permissions": { "env": true, "read": ["./data"] } } }"#,
        );
        let (s, warns) = read_synth(&cwd, None);
        assert!(s.contains("allow-env=*"), "{s}");
        assert!(s.contains("allow-read=./data"), "{s}");
        assert!(!has_note(&warns, "no permission source found"), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn package_json_inline_all_is_baked() {
        let cwd = scratch_dir("pkgall");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "package.json",
            r#"{ "name": "t", "inka": { "permissions": "all" } }"#,
        );
        let (s, _) = read_synth(&cwd, None);
        assert!(s.contains("permissions=all"), "{s}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn package_json_inline_set_name_is_baked() {
        let cwd = scratch_dir("pkgset");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "package.json",
            r#"{ "name": "t", "permissions": { "server": { "net": true } }, "inka": { "permissions": "server" } }"#,
        );
        let (s, _) = read_synth(&cwd, None);
        assert!(s.contains("allow-net=*"), "{s}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // deno.json wins over package.json when both carry an inka.permissions marker.
    #[test]
    fn deno_json_marker_wins_over_package_json() {
        let cwd = scratch_dir("markerprecedence");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "package.json",
            r#"{ "name": "t", "inka": { "permissions": { "env": true } } }"#,
        );
        write(
            &cwd,
            "deno.json",
            r#"{ "inka": { "permissions": { "net": true } } }"#,
        );
        let (s, _) = read_synth(&cwd, None);
        assert!(s.contains("allow-net=*"), "{s}");
        assert!(!s.contains("allow-env"), "deno.json should win: {s}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn desktop_config_reads_deno_json_block() {
        let cwd = scratch_dir("desktop-config");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{
  "version": "1.2.3",
  "desktop": {
    "app": {
      "name": "Acme Mail",
      "identifier": "com.acme.mail",
      "icons": { "linux": "assets/icon.png", "windows": "assets/icon.ico" }
    },
    "backend": "cef",
    "output": { "linux": "dist/mail", "windows": "dist/mail-win" },
    "release": { "baseUrl": "https://dl.acme.test/mail" },
    "errorReporting": { "url": "https://err.acme.test" }
  }
}"#,
        );
        let (cfg, warns) = desktop_config(&cwd);
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(cfg.app_name.as_deref(), Some("Acme Mail"));
        assert_eq!(cfg.identifier.as_deref(), Some("com.acme.mail"));
        assert_eq!(
            cfg.icon_linux,
            Some(DesktopIcon::Single("assets/icon.png".into()))
        );
        assert_eq!(
            cfg.icon_windows,
            Some(DesktopIcon::Single("assets/icon.ico".into()))
        );
        assert_eq!(cfg.backend.as_deref(), Some("cef"));
        assert_eq!(cfg.output_linux.as_deref(), Some("dist/mail"));
        assert_eq!(cfg.output_windows.as_deref(), Some("dist/mail-win"));
        assert_eq!(
            cfg.release_base.as_deref(),
            Some("https://dl.acme.test/mail")
        );
        assert_eq!(
            cfg.error_reporting.as_deref(),
            Some("https://err.acme.test")
        );
        assert_eq!(cfg.version.as_deref(), Some("1.2.3"));
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn desktop_config_icon_array_becomes_set() {
        let cwd = scratch_dir("desktop-icon-array");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{ "desktop": { "app": { "icons": { "linux": [
              { "path": "icon-32.png", "size": 32 },
              { "path": "icon-256.png", "size": 256 }
            ] } } } }"#,
        );
        let (cfg, warns) = desktop_config(&cwd);
        assert_eq!(
            cfg.icon_linux,
            Some(DesktopIcon::Set(vec![
                ("icon-32.png".into(), 32),
                ("icon-256.png".into(), 256),
            ]))
        );
        assert!(warns.is_empty(), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn desktop_config_falls_back_to_package_json() {
        let cwd = scratch_dir("desktop-pkg-fallback");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "package.json",
            r#"{ "name": "t", "inka": { "desktop": { "app": { "name": "PkgApp" }, "backend": "raw" } } }"#,
        );
        let (cfg, warns) = desktop_config(&cwd);
        assert_eq!(cfg.app_name.as_deref(), Some("PkgApp"));
        assert_eq!(cfg.backend.as_deref(), Some("raw"));
        assert!(warns.is_empty(), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn desktop_config_warns_on_malformed_fields() {
        let cwd = scratch_dir("desktop-malformed");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{ "desktop": { "app": { "name": 7 }, "backend": [] } }"#,
        );
        let (cfg, warns) = desktop_config(&cwd);
        assert_eq!(cfg.app_name, None);
        assert_eq!(cfg.backend, None);
        assert!(has_note(&warns, "desktop.app.name"), "{warns:?}");
        assert!(has_note(&warns, "desktop.backend"), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn desktop_config_absent_is_empty() {
        let cwd = scratch_dir("desktop-absent");
        let _ = std::fs::remove_dir_all(&cwd);
        std::fs::create_dir_all(&cwd).unwrap();
        let (cfg, warns) = desktop_config(&cwd);
        assert_eq!(
            cfg,
            DesktopConfig {
                base_dir: cwd.clone(),
                ..Default::default()
            }
        );
        assert!(warns.is_empty(), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn desktop_config_discovers_parent_project() {
        let root = scratch_dir("desktop-discover");
        let _ = std::fs::remove_dir_all(&root);
        write(
            &root,
            "deno.json",
            r#"{ "desktop": { "app": { "name": "ParentApp" } } }"#,
        );
        let sub = root.join("src/nested");
        std::fs::create_dir_all(&sub).unwrap();
        let (cfg, warns) = desktop_config(&sub);
        assert_eq!(cfg.app_name.as_deref(), Some("ParentApp"));
        assert_eq!(cfg.base_dir, root);
        assert!(warns.is_empty(), "{warns:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn desktop_config_parses_windows_and_warns_on_macos() {
        let cwd = scratch_dir("desktop-platform-only");
        let _ = std::fs::remove_dir_all(&cwd);
        write(
            &cwd,
            "deno.json",
            r#"{ "desktop": {
              "output": { "macos": "dist/App.app" },
              "app": { "icons": { "windows": "icon.ico" } }
            } }"#,
        );
        let (cfg, warns) = desktop_config(&cwd);
        assert_eq!(cfg.output_linux, None);
        // Windows keys are parsed (and valid side by side with linux ones).
        assert_eq!(cfg.output_windows, None);
        assert_eq!(
            cfg.icon_windows,
            Some(DesktopIcon::Single("icon.ico".into()))
        );
        // Only the unimplemented macOS output warns; the windows icon is used,
        // so it does not.
        assert!(has_note(&warns, "desktop.output.macos"), "{warns:?}");
        assert!(!has_note(&warns, "desktop.app.icons.macos"), "{warns:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }
}

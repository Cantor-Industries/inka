// inka vendor: per-project vendoring of packages not covered by the default store.
//
//   inka install             vendor every root declared in package.json/deno.json
//   inka install <pkg[@ver]>… vendor the given packages (deno-install style)
//   inka add <pkg[@ver]>     vendor a package (npm identity or jsr:@scope/name)
//   inka remove <pkg>        un-vendor a package (+ prune orphaned vendored deps)
//   inka vendor list|status  show the vendored set / store coverage
//   inka vendor release|ignore  git posture for the vendored/ folder
//
// Model (see plan.md §"default store + name-keyed per-project vendoring"):
//   vendored/ holds NAME-KEYED package ROOTS, no node_modules anywhere:
//       vendored/ws/  vendored/@effect/platform/  vendored/@jsr/std__assert/
//   Each entry is a package root (files + exports). Bare imports in app/vendored
//   code resolve: vendored/<name> -> default store -> builtins (store-internal
//   imports never consult vendored/). One version per vendored name is enforced.
//   vendored.lock pins the whole vendored closure (roots + auto-vendored deps).
//   package.json + deno.json (union) declare the user ROOT set (direct adds only).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::pkg;

const VENDOR_DIR: &str = "vendored";
const LOCK_FILE: &str = "vendored.lock";

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1);
}

pub(crate) fn cmd_vendor(args: &[String]) {
    let help = "usage:\n  inka install [pkg[@ver]...]   vendor this project's dependencies\n  inka add <pkg[@ver]>            vendor a package (or `inka vendor add …`)\n  inka remove <pkg>               un-vendor a package (or `inka vendor remove …`)\n  inka vendor list                show vendored packages\n  inka vendor status              vendored + default-store coverage\n  inka vendor release|ignore      git posture for vendored/ (commit vs ignore)";
    if args.is_empty() {
        eprintln!("{help}");
        std::process::exit(2);
    }
    match args[0].as_str() {
        "add" => cmd_add(&args[1..]),
        "remove" => cmd_remove(&args[1..]),
        "list" => cmd_list(&args[1..]),
        "status" => cmd_status(&args[1..]),
        "release" => cmd_git_posture(false, &args[1..]),
        "ignore" => cmd_git_posture(true, &args[1..]),
        "--help" | "-h" | "help" => {
            eprintln!("{help}");
        }
        other => {
            eprintln!("error: unknown `inka vendor` subcommand '{other}'\n\n{help}");
            std::process::exit(2);
        }
    }
}

// ---- project + store paths ------------------------------------------------

pub(crate) fn vendor_root() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(VENDOR_DIR)
}

pub(crate) fn store_dir() -> PathBuf {
    if let Ok(s) = std::env::var("INKA_STORE") {
        PathBuf::from(s)
    } else {
        crate::default_store_dir()
    }
}

/// Identity: the npm-style name a package lives under in vendored/ (jsr packages
/// use their npm-mirror identity @jsr/scope__name, as npm installs them).
fn npm_identity(spec_name: &str) -> String {
    let n = spec_name.trim();
    if let Some(rest) = n.strip_prefix("jsr:") {
        let rest = rest.trim();
        if let Some((scope, pkg)) = rest.split_once('/') {
            let pkg = pkg.split(['@', '/']).next().unwrap_or(pkg);
            let scope = scope.strip_prefix('@').unwrap_or(scope);
            return format!("@jsr/{scope}__{pkg}");
        }
    }
    let body = n.strip_prefix("npm:").unwrap_or(n);
    if let Some((scope, rest)) = body.strip_prefix('@').and_then(|b| b.split_once('/')) {
        let name = rest.split(['@', '/']).next().unwrap_or(rest);
        format!("@{scope}/{name}")
    } else {
        body.split(['@', '/']).next().unwrap_or(body).to_string()
    }
}

fn valid_version(s: &str) -> bool {
    let mut parts = s.split('.');
    let a = parts.next().and_then(|p| p.parse::<u64>().ok());
    let b = parts.next().and_then(|p| p.parse::<u64>().ok());
    let c = parts.next().and_then(|p| p.parse::<u64>().ok());
    a.is_some() && b.is_some() && c.is_some() && parts.next().is_none()
}

/// Exact version requirement from a spec string (None = "whatever resolves").
/// Only exact x.y.z is accepted; project pins are exact.
fn spec_req(spec_name: &str) -> Result<Option<String>, String> {
    let n = spec_name.trim();
    let body = n
        .strip_prefix("npm:")
        .or_else(|| n.strip_prefix("jsr:"))
        .unwrap_or(n);
    let Some(idx) = body.rfind('@') else {
        return Ok(None);
    };
    if idx == 0 {
        return Ok(None); // "@scope/name" — the only '@' is the scope marker
    }
    let ver = &body[idx + 1..];
    if ver.is_empty() {
        return Ok(None);
    }
    if !valid_version(ver) {
        return Err(format!(
            "version '{ver}' must be an exact x.y.z (inka pins vendored packages exactly)"
        ));
    }
    Ok(Some(ver.to_string()))
}

/// Does the default store already provide `name` (optionally at an exact `req`)?
fn store_satisfies(store: &Path, name: &str, req: Option<&str>) -> bool {
    let dir = store.join("node_modules").join(name);
    let raw = match fs::read(dir.join("package.json")) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let v: Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let Some(installed) = v.get("version").and_then(Value::as_str) else {
        return false;
    };
    match req {
        None => true,
        Some(r) => installed == r.trim_start_matches('^').trim_start_matches('='),
    }
}

/// Does the default store actually contain installed packages? A store without
/// a `node_modules`, or whose `node_modules` holds no package root (bare at
/// `node_modules/<name>` or scoped at `node_modules/@scope/<name>`), cannot
/// provide anything — vendoring must carry the full closure.
fn store_has_packages(store: &Path) -> bool {
    let nm = store.join("node_modules");
    if !nm.is_dir() {
        return false;
    }
    let Ok(rd) = fs::read_dir(&nm) else {
        return false;
    };
    for ent in rd.flatten() {
        let p = ent.path();
        if p.join("package.json").is_file() {
            return true;
        }
        if ent.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
            // possible scope container (node_modules/@scope/<name>/package.json)
            if let Ok(sub) = fs::read_dir(&p) {
                if sub
                    .flatten()
                    .any(|e| e.path().join("package.json").is_file())
                {
                    return true;
                }
            }
        }
    }
    false
}

// ---- spec parsing ----------------------------------------------------------

struct AddSpec {
    /// The canonical identity this package lives under everywhere inka records
    /// it (vendored dir name, `package.json` dependency key, `deno.json`
    /// imports key, `vendored.lock` entries, remove matching). jsr packages use
    /// their npm-mirror identity (`jsr:@scope/pkg` -> `@jsr/scope__pkg`), which
    /// is exactly what npm installs; `npm_identity()` is idempotent, so both
    /// the original (`jsr:@scope/pkg`) and mirror (`@jsr/scope__pkg`) spellings
    /// resolve here.
    name: String,
    req: Option<String>,
    /// npm install target (identity + optional @version)
    target: String,
    /// True when the requirement came from a declared range (`^1.2`, `~1.2.3`,
    /// `1.x`, `>=…`): the concrete version is only known after the scratch
    /// install, so store dedupe happens then instead of up front.
    declared_range: bool,
}

fn parse_add_spec(raw: &str) -> Result<AddSpec, String> {
    if raw.trim().is_empty() {
        return Err("empty package specifier".into());
    }
    let name = npm_identity(raw);
    if name.starts_with('/') || name.ends_with('/') || name.is_empty() {
        return Err(format!("invalid package name '{raw}'"));
    }
    let req = spec_req(raw)?;
    let target = match req.as_deref() {
        Some(v) => format!("{name}@{v}"),
        None => name.clone(),
    };
    Ok(AddSpec {
        name,
        req,
        target,
        declared_range: false,
    })
}

/// Build a spec from a package identity + requirement (exact, range, or empty).
fn build_spec(base: &str, req: &str) -> Result<AddSpec, String> {
    let name = npm_identity(base);
    if name.starts_with('/') || name.ends_with('/') || name.is_empty() {
        return Err(format!("invalid package name '{base}'"));
    }
    let req = req.trim();
    if req.is_empty() || req == "*" || req == "latest" {
        Ok(AddSpec {
            target: name.clone(),
            name,
            req: None,
            declared_range: false,
        })
    } else if valid_version(req.trim_start_matches('=')) {
        let exact = req.trim_start_matches('=').to_string();
        Ok(AddSpec {
            target: format!("{name}@{exact}"),
            name,
            req: Some(exact),
            declared_range: false,
        })
    } else {
        Ok(AddSpec {
            target: format!("{name}@{req}"),
            name,
            req: None,
            declared_range: true,
        })
    }
}

/// Parse a `deno.json` `imports` value (e.g. `npm:zod@3.23.8`,
/// `jsr:@std/assert@0.221.0`). Non-package specifiers are rejected.
fn parse_import_spec(raw: &str) -> Result<AddSpec, String> {
    let raw = raw.trim();
    if raw.is_empty()
        || raw.starts_with("./")
        || raw.starts_with("../")
        || raw.starts_with('/')
        || raw.contains("://")
        || raw.starts_with("node:")
        || raw.starts_with("file:")
        || raw.starts_with("data:")
        || raw.starts_with("bun:")
    {
        return Err(format!("not a package specifier: {raw}"));
    }
    let (base, req) = match raw.rfind('@') {
        Some(i) if i > 0 => (&raw[..i], &raw[i + 1..]),
        _ => (raw, ""),
    };
    build_spec(base, req)
}

/// Roots declared by this project: `package.json` `dependencies` plus
/// `deno.json` `imports` (deno wins on conflict), sorted by canonical name.
/// `devDependencies` are intentionally ignored in v1.
fn declared_root_specs(cwd: &Path) -> Result<Vec<AddSpec>, String> {
    let mut out: BTreeMap<String, AddSpec> = BTreeMap::new();
    let mut notes: Vec<String> = Vec::new();

    let pkg = cwd.join("package.json");
    if pkg.is_file() {
        if let Ok(raw) = fs::read_to_string(&pkg) {
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                if let Some(deps) = v.get("dependencies").and_then(Value::as_object) {
                    for (name, val) in deps {
                        let req = val.as_str().unwrap_or("");
                        match build_spec(name, req) {
                            Ok(s) => {
                                out.insert(s.name.clone(), s);
                            }
                            Err(e) => notes.push(format!("package.json dependency '{name}': {e}")),
                        }
                    }
                }
            }
        }
    }

    let deno = cwd.join("deno.json");
    if deno.is_file() {
        if let Ok(raw) = fs::read_to_string(&deno) {
            let text = crate::config::strip_jsonc(&raw);
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(imports) = v.get("imports").and_then(Value::as_object) {
                    for val in imports.values() {
                        let Some(spec) = val.as_str() else { continue };
                        match parse_import_spec(spec) {
                            Ok(s) => {
                                out.insert(s.name.clone(), s);
                            }
                            Err(e) => notes.push(format!("deno.json imports '{spec}': {e}")),
                        }
                    }
                }
            }
        }
    }

    for n in notes {
        eprintln!("[inka] note: skipped {n}");
    }
    Ok(out.into_values().collect())
}

// ---- lock file -------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
struct Lock {
    #[serde(default)]
    entries: BTreeMap<String, LockEntry>,
    /// Informational record of the default store used for dedupe at add time
    /// (WS3-3). Never gates anything; `cmd_status` warns when the current store
    /// identity differs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    store: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct LockEntry {
    version: String,
    #[serde(default)]
    why: String, // "root" | "dep"
    #[serde(default)]
    converted: Vec<String>,
}

fn lock_path(root: &Path) -> PathBuf {
    root.join(LOCK_FILE)
}

fn load_lock(root: &Path) -> Lock {
    let p = lock_path(root);
    match fs::read(&p) {
        Ok(raw) => serde_json::from_slice(&raw).unwrap_or_default(),
        Err(_) => Lock::default(),
    }
}

fn save_lock(root: &Path, lock: &Lock) -> Result<(), String> {
    let json = serde_json::to_string_pretty(lock).map_err(|e| format!("encode lock: {e}"))?;
    fs::write(lock_path(root), format!("{json}\n"))
        .map_err(|e| format!("cannot write {}: {e}", lock_path(root).display()))
}

/// Identity string for the default store, for the lock's informational note:
/// the store path plus its `seed-manifest.json` `sha256` record when present.
fn store_identity(store: &Path) -> String {
    let sha = fs::read(store.join("seed-manifest.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.get("sha256").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    let sha = if sha.is_empty() {
        "no-sha-record".to_string()
    } else {
        sha
    };
    format!("{} sha256={sha}", store.display())
}

// ---- manifests (package.json + deno.json union) ---------------------------

struct Manifests {
    pkg_json: Option<PathBuf>,
    deno_json: Option<PathBuf>,
}

fn project_manifests() -> Manifests {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    Manifests {
        pkg_json: {
            let p = cwd.join("package.json");
            p.is_file().then_some(p)
        },
        deno_json: {
            let p = cwd.join("deno.json");
            p.is_file().then_some(p)
        },
    }
}

/// Ensure at least one manifest exists (create package.json when neither does).
fn ensure_manifest(manifests: &mut Manifests) {
    if manifests.pkg_json.is_none() && manifests.deno_json.is_none() {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let p = cwd.join("package.json");
        let name = cwd
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "app".to_string());
        let v = serde_json::json!({ "name": name, "private": true, "dependencies": {} });
        if fs::write(
            &p,
            format!("{}\n", serde_json::to_string_pretty(&v).unwrap()),
        )
        .is_ok()
        {
            manifests.pkg_json = Some(p);
        }
    }
}

fn read_json(path: &Path) -> Result<Value, String> {
    let raw =
        fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| {
        format!(
            "{} is not plain JSON ({e}); deno.json with comments isn't writable yet",
            path.display()
        )
    })
}

fn write_json(path: &Path, v: &Value) -> Result<(), String> {
    let json = serde_json::to_string_pretty(v).map_err(|e| format!("encode: {e}"))?;
    fs::write(path, format!("{json}\n"))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Declare `name@version` in whichever manifests exist (union kept consistent).
fn manifests_add(manifests: &Manifests, name: &str, version: &str) -> Result<(), String> {
    if let Some(p) = &manifests.pkg_json {
        let mut v = read_json(p)?;
        let deps = v
            .get_mut("dependencies")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| format!("{} has no `dependencies` object", p.display()))?;
        deps.insert(name.to_string(), Value::String(version.to_string()));
        write_json(p, &v)?;
    }
    if let Some(p) = &manifests.deno_json {
        let mut v = read_json(p)?;
        let obj = v
            .as_object_mut()
            .ok_or_else(|| "deno.json must be an object".to_string())?;
        let imports = obj
            .entry("imports")
            .or_insert_with(|| Value::Object(Default::default()));
        let imports = imports
            .as_object_mut()
            .ok_or_else(|| "deno.json `imports` must be an object".to_string())?;
        imports.insert(
            name.to_string(),
            Value::String(format!("npm:{name}@{version}")),
        );
        write_json(p, &v)?;
    }
    Ok(())
}

fn manifests_remove(manifests: &Manifests, name: &str) -> Result<(), String> {
    if let Some(p) = &manifests.pkg_json {
        if let Ok(mut v) = read_json(p) {
            if let Some(deps) = v.get_mut("dependencies").and_then(Value::as_object_mut) {
                deps.remove(name);
            }
            write_json(p, &v)?;
        }
    }
    if let Some(p) = &manifests.deno_json {
        if let Ok(mut v) = read_json(p) {
            if let Some(imports) = v.get_mut("imports").and_then(Value::as_object_mut) {
                imports.remove(name);
            }
            write_json(p, &v)?;
        }
    }
    Ok(())
}

/// Is `name` declared as a root in either manifest?
fn manifests_declares(manifests: &Manifests, name: &str) -> bool {
    for p in [&manifests.pkg_json, &manifests.deno_json]
        .into_iter()
        .flatten()
    {
        if let Ok(v) = read_json(p) {
            if let Some(deps) = v.get("dependencies").and_then(Value::as_object) {
                if deps.contains_key(name) {
                    return true;
                }
            }
            if let Some(imports) = v.get("imports").and_then(Value::as_object) {
                if imports.contains_key(name) {
                    return true;
                }
            }
        }
    }
    false
}

// ---- npm scratch install ---------------------------------------------------

fn jsr_npmrc(work: &Path) {
    let _ = fs::write(work.join(".npmrc"), "@jsr:registry=https://npm.jsr.io\n");
}

/// Install `target` alone in a scratch dir; return the dir whose `node_modules`
/// holds the resolved closure (network only here).
fn scratch_install(target: &str) -> Result<PathBuf, String> {
    let work = std::env::temp_dir().join(format!("inka-vendor-{}", std::process::id()));
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work).map_err(|e| format!("cannot create workdir: {e}"))?;
    jsr_npmrc(&work);
    let mut cmd = Command::new("npm");
    cmd.current_dir(&work)
        .args(["install", "--no-save", "--omit=dev", target]);
    pkg::run_ok(&mut cmd, "npm install").map_err(|e| {
        let _ = fs::remove_dir_all(&work);
        e
    })?;
    if !work.join("node_modules").is_dir() {
        let _ = fs::remove_dir_all(&work);
        return Err("npm install did not produce a node_modules directory".into());
    }
    Ok(work)
}

/// All (name, version) present anywhere in a node_modules tree (hoisted + nested).
fn collect_instances(nm: &Path, out: &mut BTreeMap<String, BTreeSet<String>>) {
    let Ok(top) = fs::read_dir(nm) else { return };
    let mut entries: Vec<PathBuf> = top
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();
    for dir in entries {
        let name = dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if name.starts_with('.') {
            continue;
        }
        let pkgs: Vec<(String, PathBuf)> = if name.starts_with('@') {
            let mut v = Vec::new();
            if let Ok(sub) = fs::read_dir(&dir) {
                let mut subs: Vec<PathBuf> = sub
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect();
                subs.sort();
                for p in subs {
                    let pkg = p
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    v.push((format!("{name}/{pkg}"), p));
                }
            }
            v
        } else {
            vec![(name.clone(), dir)]
        };
        for (full, pkg_dir) in pkgs {
            if let Ok(raw) = fs::read(pkg_dir.join("package.json")) {
                if let Ok(val) = serde_json::from_slice::<Value>(&raw) {
                    if let Some(ver) = val.get("version").and_then(Value::as_str) {
                        out.entry(full).or_default().insert(ver.to_string());
                    }
                }
            }
            let nested = pkg_dir.join("node_modules");
            if nested.is_dir() {
                collect_instances(&nested, out);
            }
        }
    }
}

/// Resolve the version npm installed for a top-level package in a scratch tree.
fn installed_version(nm: &Path, name: &str) -> Option<String> {
    let raw = fs::read(nm.join(name).join("package.json")).ok()?;
    let v: Value = serde_json::from_slice(&raw).ok()?;
    v.get("version").and_then(Value::as_str).map(str::to_string)
}

/// Copy a package root's files (no nested node_modules) to `dest/<name>`.
fn copy_package_root(nm: &Path, name: &str, dest: &Path) -> Result<(), String> {
    let src = nm.join(name);
    let dst = dest.join(name);
    let _ = fs::remove_dir_all(&dst);
    copy_tree(&src, &dst)?;
    // never carry a package's own nested node_modules into the flat pool
    let _ = fs::remove_dir_all(dst.join("node_modules"));
    Ok(())
}

fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| format!("cannot create {}: {e}", dst.display()))?;
    let entries = fs::read_dir(src).map_err(|e| format!("cannot read {}: {e}", src.display()))?;
    for ent in entries.flatten() {
        let s = ent.path();
        let name = ent.file_name();
        let d = dst.join(name);
        let ft = ent
            .file_type()
            .map_err(|e| format!("stat {}: {e}", s.display()))?;
        if ft.is_dir() {
            copy_tree(&s, &d)?;
        } else if ft.is_symlink() {
            let target = fs::read_link(&s).map_err(|e| format!("readlink {}: {e}", s.display()))?;
            std::os::unix::fs::symlink(&target, &d)
                .map_err(|e| format!("symlink {}: {e}", d.display()))?;
        } else {
            fs::copy(&s, &d).map_err(|e| format!("copy {}: {e}", s.display()))?;
        }
    }
    Ok(())
}

// ---- add -------------------------------------------------------------------

pub(crate) fn cmd_add(args: &[String]) {
    let (force, specs) = parse_add_flags(args, "usage: inka add <pkg[@ver]> [--force]");
    if specs.is_empty() {
        fail("inka add needs a package name");
    }
    notice_missing_store();
    for raw in &specs {
        let spec = parse_add_spec(raw).unwrap_or_else(|e| fail(&e));
        add_one(force, &spec);
    }
}

const INSTALL_HELP: &str = "usage: inka install [pkg[@ver]...] [--force] [--prod]\n\
     \x20 no packages: vendor every root declared in package.json (dependencies)\n\
     \x20              and deno.json (imports)\n\
     \x20 packages:    vendor the given packages (same as `inka add`)";

/// `inka install`: vendor this project's dependencies into `vendored/`.
pub(crate) fn cmd_install(args: &[String]) {
    let (force, specs) = parse_add_flags(args, INSTALL_HELP);
    if specs.is_empty() {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let declared = declared_root_specs(&cwd).unwrap_or_else(|e| fail(&e));
        if declared.is_empty() {
            println!("[inka] no dependencies declared in package.json or deno.json");
            return;
        }
        notice_missing_store();
        for spec in &declared {
            add_one(force, spec);
        }
    } else {
        notice_missing_store();
        for raw in &specs {
            let spec = parse_add_spec(raw).unwrap_or_else(|e| fail(&e));
            add_one(force, &spec);
        }
    }
}

/// Parse `--force`/`-f`, `--prod` (accepted no-op) and collect positional specs.
fn parse_add_flags(args: &[String], help: &str) -> (bool, Vec<String>) {
    let mut force = false;
    let mut specs: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--force" | "-f" => force = true,
            "--prod" => {} // v1 reads prod deps only; accepted for deno parity
            "--help" | "-h" => {
                eprintln!("{help}");
                std::process::exit(0);
            }
            other => specs.push(other.to_string()),
        }
    }
    (force, specs)
}

/// Surface a missing/empty default store once per command.
fn notice_missing_store() {
    let store = store_dir();
    if !store_has_packages(&store) {
        println!(
            "[inka] no default store installed ({}); dependencies it would normally provide \
             will be vendored in full. Install one with: inka update",
            store.display()
        );
    }
}

fn add_one(force: bool, spec: &AddSpec) {
    let store = store_dir();
    let root = vendor_root();

    // 0) already vendored (idempotent) — unless a new exact version or --force
    {
        let lock = load_lock(&root);
        if let Some(cur) = lock.entries.get(&spec.name) {
            if !force {
                match spec.req.as_deref() {
                    None => {
                        println!(
                            "[inka] '{}' is already vendored at {} (use '{}' or --force to refresh)",
                            spec.name, cur.version, cur.version
                        );
                        return;
                    }
                    Some(r) if r == cur.version => {
                        println!(
                            "[inka] '{}' is already vendored at {}",
                            spec.name, cur.version
                        );
                        return;
                    }
                    Some(_) => {} // version change: refresh below
                }
            }
        }
    }

    // 0.5) missing/empty default store is surfaced once by the caller.

    // 1) dedupe: default store already satisfies -> skip (unless --force).
    //    Declared ranges bypass this: their concrete version is only known
    //    after the scratch install below.
    if !force && !spec.declared_range && store_satisfies(&store, &spec.name, spec.req.as_deref()) {
        println!(
            "[inka] '{}' is already provided by the default store; nothing vendored (use --force to vendor anyway)",
            spec.name
        );
        return;
    }

    // 2) install the package alone (network) to learn its resolved closure
    let work = scratch_install(&spec.target).unwrap_or_else(|e| fail(&e));
    let nm = work.join("node_modules");

    // 3) detect flatten conflicts (one version per vendored name)
    let mut instances: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    collect_instances(&nm, &mut instances);
    let conflicts: Vec<String> = instances
        .iter()
        .filter(|(_, vers)| vers.len() > 1)
        .map(|(n, vers)| {
            format!(
                "{n} ({})",
                vers.iter().cloned().collect::<Vec<_>>().join(", ")
            )
        })
        .collect();
    if !conflicts.is_empty() {
        let _ = fs::remove_dir_all(&work);
        fail(&format!(
            "the vendored set cannot flatten version conflicts for: {}\n  pin compatible versions or keep the conflicting copy in the default store",
            conflicts.join("; ")
        ));
    }

    // 4) decide which closure members to vendor: requested root always; other
    //    members only when the default store cannot satisfy their exact version.
    let requested_ver = installed_version(&nm, &spec.name)
        .ok_or_else(|| {
            let _ = fs::remove_dir_all(&work);
            format!("npm did not install '{}'", spec.name)
        })
        .unwrap_or_else(|e| fail(&e));

    // A declared range resolves to a concrete version; if the store already
    // provides exactly that, treat it as store-provided (nothing to vendor).
    if !force && spec.declared_range && store_satisfies(&store, &spec.name, Some(&requested_ver)) {
        let _ = fs::remove_dir_all(&work);
        println!(
            "[inka] '{}' is already provided by the default store; nothing vendored (use --force to vendor anyway)",
            spec.name
        );
        return;
    }

    let mut to_vendor: Vec<(String, String, String)> = Vec::new(); // (name, ver, why)
    to_vendor.push((spec.name.clone(), requested_ver.clone(), "root".to_string()));
    let mut names: Vec<String> = instances.keys().cloned().collect();
    names.sort();
    for name in names {
        if name == spec.name {
            continue;
        }
        let ver = instances[&name].iter().next().cloned().unwrap_or_default();
        if !store_satisfies(&store, &name, Some(&ver)) {
            to_vendor.push((name, ver, "dep".to_string()));
        }
    }

    // 5) place roots under vendored/ (raw package files; the engine runs CJS
    //    natively, so no CJS->ESM conversion happens here).
    fs::create_dir_all(&root)
        .unwrap_or_else(|e| fail(&format!("cannot create {}: {e}", root.display())));
    for (name, _ver, _why) in &to_vendor {
        copy_package_root(&nm, name, &root).unwrap_or_else(|e| {
            let _ = fs::remove_dir_all(&work);
            fail(&e)
        });
    }
    let _ = fs::remove_dir_all(&work);

    // 7) lock + manifests
    let mut lock = load_lock(&root);
    for (name, ver, why) in &to_vendor {
        let entry = lock.entries.entry(name.clone()).or_insert(LockEntry {
            version: ver.clone(),
            why: why.clone(),
            converted: Vec::new(),
        });
        entry.version = ver.clone();
        if why == "root" {
            entry.why = "root".to_string();
        } else if entry.why != "root" {
            entry.why = "dep".to_string();
        }
    }
    // WS3-3: record which default store the dedupe consulted.
    lock.store = Some(store_identity(&store));
    save_lock(&root, &lock).unwrap_or_else(|e| fail(&e));

    let mut manifests = project_manifests();
    ensure_manifest(&mut manifests);
    manifests_add(&manifests, &spec.name, &requested_ver).unwrap_or_else(|e| fail(&e));

    // a version refresh may leave dep entries no other vendored package needs
    prune_orphans(&root, &mut lock);
    save_lock(&root, &lock).unwrap_or_else(|e| fail(&e));

    git_ignore_ensure(&root);
    println!(
        "[inka] vendored {}@{} (+{} dependency entr{} into {})",
        spec.name,
        requested_ver,
        to_vendor.len() - 1,
        if to_vendor.len() == 2 { "y" } else { "ies" },
        root.display()
    );
    for (name, ver, why) in &to_vendor {
        println!("  vendored {name}@{ver} ({why})");
    }
}

// ---- remove ----------------------------------------------------------------

pub(crate) fn cmd_remove(args: &[String]) {
    if args.is_empty() {
        fail("inka remove needs a package name");
    }
    if args[0] == "--help" || args[0] == "-h" {
        eprintln!("usage: inka remove <pkg>");
        std::process::exit(0);
    }
    let spec = parse_add_spec(&args[0]).unwrap_or_else(|e| fail(&e));
    let store = store_dir();
    let root = vendor_root();
    let pkg_dir = root.join(&spec.name);

    if !pkg_dir.is_dir() {
        if store_satisfies(&store, &spec.name, None) {
            println!(
                "[inka] '{}' is provided by the default store, not vendored; nothing to remove",
                spec.name
            );
        } else {
            fail(&format!(
                "'{}' is neither vendored nor in the default store",
                spec.name
            ));
        }
        return;
    }

    let _ = fs::remove_dir_all(&pkg_dir);

    let mut lock = load_lock(&root);
    lock.entries.remove(&spec.name);
    prune_orphans(&root, &mut lock);
    save_lock(&root, &lock).unwrap_or_else(|e| fail(&e));

    let manifests = project_manifests();
    manifests_remove(&manifests, &spec.name).unwrap_or_else(|e| fail(&e));
    println!("[inka] removed vendored '{}'", spec.name);
}

/// Prune vendored dep entries no longer referenced by any remaining vendored
/// package and not declared as roots in the manifests.
fn prune_orphans(root: &Path, lock: &mut Lock) {
    let manifests = project_manifests();
    loop {
        let mut changed = false;
        let names: Vec<String> = lock.entries.keys().cloned().collect();
        for name in names {
            let is_root =
                manifests_declares(&manifests, &name) || lock.entries[&name].why == "root";
            if is_root {
                continue;
            }
            let referenced = lock
                .entries
                .iter()
                .any(|(n, _)| n.as_str() != name.as_str() && package_requires(root, &name));
            if !referenced {
                lock.entries.remove(&name);
                let _ = fs::remove_dir_all(root.join(&name));
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

/// Is `dep` a runtime dependency of any package root currently under `root`?
/// WS3-2: `dep` counts when it appears in ANY of `dependencies`,
/// `optionalDependencies`, or `peerDependencies` (skipping `peerDependenciesMeta`
/// entries marked optional=true) of an on-disk vendored package root. Only
/// affects prune/remove decisions; never auto-vendors.
fn package_requires(root: &Path, dep: &str) -> bool {
    let Ok(top) = fs::read_dir(root) else {
        return false;
    };
    for ent in top.flatten() {
        let dir = ent.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(raw) = fs::read(dir.join("package.json")) else {
            continue;
        };
        let Ok(v) = serde_json::from_slice::<Value>(&raw) else {
            continue;
        };
        let key = |k: &str| {
            v.get(k)
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default()
        };
        let deps = key("dependencies");
        if deps.contains_key(dep) {
            return true;
        }
        if key("optionalDependencies").contains_key(dep) {
            return true;
        }
        let peers = key("peerDependencies");
        if peers.contains_key(dep) {
            // skip peers declared optional in peerDependenciesMeta
            let meta = v
                .get("peerDependenciesMeta")
                .and_then(Value::as_object)
                .and_then(|m| m.get(dep))
                .and_then(Value::as_object)
                .and_then(|m| m.get("optional"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !meta {
                return true;
            }
        }
    }
    false
}

// ---- list / status ---------------------------------------------------------

pub(crate) fn cmd_list(args: &[String]) {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!("usage: inka vendor list");
        std::process::exit(0);
    }
    let root = vendor_root();
    let lock = load_lock(&root);
    if lock.entries.is_empty() {
        println!("(no vendored packages; `inka add <pkg>` to vendor one)");
        return;
    }
    let manifests = project_manifests();
    for (name, e) in &lock.entries {
        let root_mark = if manifests_declares(&manifests, name) || e.why == "root" {
            "root"
        } else {
            "dep"
        };
        println!(
            "{name}@{version} [{root_mark}{converted}]",
            version = e.version,
            converted = if e.converted.is_empty() {
                String::new()
            } else {
                format!(", converted={}", e.converted.join("+"))
            }
        );
    }
}

pub(crate) fn cmd_status(args: &[String]) {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!("usage: inka vendor status");
        std::process::exit(0);
    }
    let root = vendor_root();
    let store = store_dir();
    let lock = load_lock(&root);
    println!("vendored dir: {}", root.display());
    println!(
        "default store: {} ({})",
        store.display(),
        if store.join("node_modules").is_dir() {
            "present"
        } else {
            "absent"
        }
    );
    // WS3-3: informational store identity recorded at add time; warn (never
    // fail) when the current store differs.
    if let Some(recorded) = &lock.store {
        let current = store_identity(&store);
        println!("recorded default store: {recorded}");
        if current != *recorded {
            eprintln!(
                "[inka] warning: this vendored set was built against a different default store \
                 ({recorded});\n  current store identity: {current}\n  reseed or vendor the \
                 affected deps to pin behavior"
            );
        }
    }
    if lock.entries.is_empty() {
        println!("vendored: (none)");
    }
    for (name, e) in &lock.entries {
        let store_state = if store_satisfies(&store, name, Some(&e.version)) {
            "also in store"
        } else if store_satisfies(&store, name, None) {
            "store has a different version"
        } else {
            "not in store"
        };
        println!("  {name}@{version} {store_state}", version = e.version);
    }
    if let Ok(gi) = fs::read_to_string(root.parent().unwrap_or(&root).join(".gitignore")) {
        let ignored = gi
            .lines()
            .any(|l| l.trim().trim_end_matches('/') == VENDOR_DIR);
        println!(
            "git posture: {} (vendored/ {})",
            if ignored {
                "ignore (dev)"
            } else {
                "commit (release)"
            },
            if ignored { "ignored" } else { "not ignored" }
        );
    } else {
        println!("git posture: no .gitignore found");
    }
}

// ---- git posture -----------------------------------------------------------

fn git_ignore_path() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    cwd.join(".gitignore")
}

/// Ensure `vendored/` is ignored (dev posture).
fn git_ignore_ensure(_root: &Path) {
    let gi = git_ignore_path();
    let existing = fs::read_to_string(&gi).unwrap_or_default();
    let has = existing
        .lines()
        .any(|l| l.trim().trim_end_matches('/') == VENDOR_DIR);
    if has {
        return;
    }
    let mut out = existing.trim_end().to_string();
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&format!("{VENDOR_DIR}/\n"));
    let _ = fs::write(&gi, out);
}

fn cmd_git_posture(ignore: bool, args: &[String]) {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!("usage: inka vendor release|ignore");
        std::process::exit(0);
    }
    if ignore {
        git_ignore_ensure(&vendor_root());
        println!("[inka] vendored/ is gitignored (dev posture)");
        return;
    }
    // release: drop the vendored/ ignore line
    let gi = git_ignore_path();
    let existing = fs::read_to_string(&gi).unwrap_or_default();
    let kept: Vec<&str> = existing
        .lines()
        .filter(|l| l.trim().trim_end_matches('/') != VENDOR_DIR)
        .collect();
    let body = if kept.is_empty() {
        String::new()
    } else {
        format!("{}\n", kept.join("\n"))
    };
    fs::write(&gi, body).unwrap_or_else(|e| {
        eprintln!("error: cannot write {}: {e}", gi.display());
        std::process::exit(1);
    });
    println!("[inka] vendored/ is no longer gitignored (commit it for release builds)");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(kind: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("inkavendor-{kind}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_file(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    #[test]
    fn store_without_node_modules_has_no_packages() {
        let store = scratch("no-nm");
        assert!(!store_has_packages(&store));
        let _ = fs::remove_dir_all(&store);
    }

    #[test]
    fn empty_node_modules_has_no_packages() {
        let store = scratch("empty-nm");
        fs::create_dir_all(store.join("node_modules")).unwrap();
        assert!(!store_has_packages(&store));
        let _ = fs::remove_dir_all(&store);
    }

    #[test]
    fn bare_package_counts_as_present() {
        let store = scratch("bare");
        write_file(
            &store,
            "node_modules/zod/package.json",
            r#"{"version":"3.23.0"}"#,
        );
        assert!(store_has_packages(&store));
        let _ = fs::remove_dir_all(&store);
    }

    #[test]
    fn scoped_package_counts_as_present() {
        let store = scratch("scoped");
        write_file(
            &store,
            "node_modules/@effect/platform/package.json",
            r#"{"version":"1.0.0"}"#,
        );
        assert!(store_has_packages(&store));
        let _ = fs::remove_dir_all(&store);
    }

    #[test]
    fn junk_dirs_in_node_modules_do_not_count() {
        let store = scratch("junk");
        write_file(&store, "node_modules/README.md", "hi\n");
        write_file(&store, "node_modules/stray/file.txt", "x\n");
        assert!(!store_has_packages(&store));
        let _ = fs::remove_dir_all(&store);
    }

    // ---- WS2-3: canonical npm-mirror identity ----------------------------------

    #[test]
    fn npm_identity_uses_jsr_mirror_and_is_idempotent() {
        assert_eq!(npm_identity("jsr:@std/path"), "@jsr/std__path");
        // the mirror spelling is already canonical
        assert_eq!(npm_identity("@jsr/std__path"), "@jsr/std__path");
        // npm: prefix + mirror resolves the same way
        assert_eq!(npm_identity("npm:@jsr/std__path"), "@jsr/std__path");
        // a genuine npm scoped package is left as itself (no jsr rewrite)
        assert_eq!(npm_identity("@scope/pkg"), "@scope/pkg");
        assert_eq!(npm_identity("zod"), "zod");
    }

    #[test]
    fn remove_by_original_and_mirror_target_the_same_entry() {
        let original = parse_add_spec("jsr:@std/path").unwrap();
        let mirror = parse_add_spec("@jsr/std__path").unwrap();
        assert_eq!(original.name, "@jsr/std__path");
        assert_eq!(mirror.name, "@jsr/std__path");
    }

    // ---- WS3-2: prune considers optional/peer dependencies --------------------

    #[test]
    fn package_requires_sees_optional_and_nonoptional_peers() {
        let root = scratch("reqs");
        // a -> b only under optionalDependencies
        write_file(
            &root,
            "a/package.json",
            r#"{"name":"a","version":"1.0.0","optionalDependencies":{"b":"1.0.0"}}"#,
        );
        // a2 -> peers b (non-optional) and c (optional via peerDependenciesMeta)
        write_file(
            &root,
            "a2/package.json",
            r#"{"name":"a2","version":"1.0.0","peerDependencies":{"b":"1.0.0","c":"1.0.0"},"peerDependenciesMeta":{"c":{"optional":true}}}"#,
        );
        write_file(&root, "b/package.json", r#"{"name":"b","version":"1.0.0"}"#);
        write_file(&root, "c/package.json", r#"{"name":"c","version":"1.0.0"}"#);
        // "b" referenced via optionalDependencies and a non-optional peer
        assert!(package_requires(&root, "b"));
        // "c" only referenced as an optional peer -> not a hard reference
        assert!(!package_requires(&root, "c"));
        // an unreferenced name is not required
        assert!(!package_requires(&root, "nope"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_keeps_optional_dep_until_referrer_removed() {
        let root = scratch("prune");
        write_file(
            &root,
            "a/package.json",
            r#"{"name":"a","version":"1.0.0","optionalDependencies":{"b":"1.0.0"}}"#,
        );
        write_file(&root, "b/package.json", r#"{"name":"b","version":"1.0.0"}"#);
        let mut lock = Lock::default();
        lock.entries.insert(
            "a".to_string(),
            LockEntry {
                version: "1.0.0".into(),
                why: "root".into(),
                converted: vec![],
            },
        );
        lock.entries.insert(
            "b".to_string(),
            LockEntry {
                version: "1.0.0".into(),
                why: "dep".into(),
                converted: vec![],
            },
        );
        // A still references B (optional) -> B is not pruned.
        prune_orphans(&root, &mut lock);
        assert!(lock.entries.contains_key("b"));
        // Remove A (dir + lock entry), then B becomes an orphan and is pruned.
        let _ = fs::remove_dir_all(root.join("a"));
        lock.entries.remove("a");
        prune_orphans(&root, &mut lock);
        assert!(!lock.entries.contains_key("b"));
        assert!(!root.join("b").exists());
        let _ = fs::remove_dir_all(&root);
    }

    // ---- WS3-3: lock store note -----------------------------------------------

    #[test]
    fn lock_store_note_round_trips_and_defaults_backward_compatibly() {
        let mut lock = Lock::default();
        lock.store = Some("/tmp/store sha256=abc123".to_string());
        lock.entries.insert(
            "zod".to_string(),
            LockEntry {
                version: "3.23.0".into(),
                why: "root".into(),
                converted: vec![],
            },
        );
        let json = serde_json::to_string(&lock).unwrap();
        assert!(json.contains("\"store\""), "{json}");
        let back: Lock = serde_json::from_str(&json).unwrap();
        assert_eq!(back.store.as_deref(), Some("/tmp/store sha256=abc123"));
        assert_eq!(back.entries.len(), 1);

        // A pre-WS3-3 lock (no "store" key) still parses with store == None.
        let old = r#"{"entries":{"zod":{"version":"3.23.0","why":"root","converted":[]}}}"#;
        let parsed: Lock = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.store, None);
        assert_eq!(parsed.entries.len(), 1);
    }

    // ---- install: declared-root discovery ---------------------------------

    #[test]
    fn build_spec_classifies_exact_range_and_none() {
        let exact = build_spec("zod", "3.23.8").unwrap();
        assert_eq!(exact.name, "zod");
        assert_eq!(exact.req.as_deref(), Some("3.23.8"));
        assert_eq!(exact.target, "zod@3.23.8");
        assert!(!exact.declared_range);

        let ranged = build_spec("zod", "^3.23.0").unwrap();
        assert_eq!(ranged.name, "zod");
        assert_eq!(ranged.req, None);
        assert_eq!(ranged.target, "zod@^3.23.0");
        assert!(ranged.declared_range);

        let none = build_spec("zod", "*").unwrap();
        assert_eq!(none.req, None);
        assert_eq!(none.target, "zod");
        assert!(!none.declared_range);
    }

    #[test]
    fn parse_import_spec_handles_npm_jsr_and_bare() {
        assert_eq!(parse_import_spec("npm:zod@3.23.8").unwrap().name, "zod");
        let jsr = parse_import_spec("jsr:@std/assert@0.221.0").unwrap();
        assert_eq!(jsr.name, "@jsr/std__assert");
        assert_eq!(jsr.req.as_deref(), Some("0.221.0"));
        assert_eq!(parse_import_spec("nanoid").unwrap().name, "nanoid");
    }

    #[test]
    fn parse_import_spec_rejects_non_packages() {
        for bad in [
            "./local.ts",
            "../up.js",
            "https://example.com/mod.ts",
            "node:fs",
            "file:///x",
        ] {
            assert!(parse_import_spec(bad).is_err(), "should reject {bad}");
        }
    }

    #[test]
    fn declared_root_specs_reads_package_and_deno_union() {
        let cwd = scratch("declared");
        write_file(
            &cwd,
            "package.json",
            r#"{"dependencies":{"zod":"3.23.8","ms":"^2.1.3"}}"#,
        );
        write_file(
            &cwd,
            "deno.json",
            r#"{
  // comments are tolerated
  "imports": {
    "zod": "npm:zod@3.22.0",
    "@std/assert": "jsr:@std/assert@0.221.0",
    "local": "./local.ts"
  }
}"#,
        );
        let specs = declared_root_specs(&cwd).unwrap();
        let by_name: BTreeMap<String, AddSpec> =
            specs.into_iter().map(|s| (s.name.clone(), s)).collect();

        // deno.json wins over package.json for zod
        assert_eq!(by_name["zod"].req.as_deref(), Some("3.22.0"));
        // package.json range is preserved as a declared range
        assert!(by_name["ms"].declared_range);
        assert_eq!(by_name["ms"].target, "ms@^2.1.3");
        // jsr import maps to its npm-mirror identity
        assert_eq!(by_name["@jsr/std__assert"].req.as_deref(), Some("0.221.0"));
        // non-package import-map entries are skipped
        assert!(!by_name.contains_key("local"));
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn declared_root_specs_empty_without_config() {
        let cwd = scratch("declared-empty");
        assert!(declared_root_specs(&cwd).unwrap().is_empty());
        let _ = fs::remove_dir_all(&cwd);
    }
}

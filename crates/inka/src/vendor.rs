// inka vendor: per-project vendoring of packages not covered by the default store.
//
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

use crate::{pkg, runtime_dir};

const VENDOR_DIR: &str = "vendored";
const LOCK_FILE: &str = "vendored.lock";

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1);
}

pub(crate) fn cmd_vendor(args: &[String]) {
    let help = "usage:\n  inka add <pkg[@ver]>            vendor a package (or `inka vendor add …`)\n  inka remove <pkg>               un-vendor a package (or `inka vendor remove …`)\n  inka vendor list                show vendored packages\n  inka vendor status              vendored + default-store coverage\n  inka vendor release|ignore      git posture for vendored/ (commit vs ignore)";
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

fn vendor_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(VENDOR_DIR)
}

fn store_dir() -> PathBuf {
    if let Ok(s) = std::env::var("INKA_STORE") {
        PathBuf::from(s)
    } else {
        runtime_dir(None).join("store")
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

// ---- spec parsing ----------------------------------------------------------

struct AddSpec {
    name: String, // npm identity (vendored dir name)
    req: Option<String>,
    /// npm install target (identity + optional @version)
    target: String,
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
    Ok(AddSpec { name, req, target })
}

// ---- lock file -------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
struct Lock {
    #[serde(default)]
    entries: BTreeMap<String, LockEntry>,
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
        if fs::write(&p, format!("{}\n", serde_json::to_string_pretty(&v).unwrap())).is_ok() {
            manifests.pkg_json = Some(p);
        }
    }
}

fn read_json(path: &Path) -> Result<Value, String> {
    let raw = fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| {
        format!(
            "{} is not plain JSON ({e}); deno.json with comments isn't writable yet",
            path.display()
        )
    })
}

fn write_json(path: &Path, v: &Value) -> Result<(), String> {
    let json = serde_json::to_string_pretty(v).map_err(|e| format!("encode: {e}"))?;
    fs::write(path, format!("{json}\n")).map_err(|e| format!("cannot write {}: {e}", path.display()))
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
        let obj = v.as_object_mut().ok_or_else(|| "deno.json must be an object".to_string())?;
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
    for p in [&manifests.pkg_json, &manifests.deno_json].into_iter().flatten() {
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
    let mut entries: Vec<PathBuf> = top.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    entries.sort();
    for dir in entries {
        let name = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let pkgs: Vec<(String, PathBuf)> = if name.starts_with('@') {
            let mut v = Vec::new();
            if let Ok(sub) = fs::read_dir(&dir) {
                let mut subs: Vec<PathBuf> = sub.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
                subs.sort();
                for p in subs {
                    let pkg = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
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
        let ft = ent.file_type().map_err(|e| format!("stat {}: {e}", s.display()))?;
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

// ---- CJS classification (mirrors the resolver's rule) ----------------------

/// True if a package root's served ESM entry is CommonJS (no usable import
/// condition, and main/.js in a non-"module" package). Mirrors inka-resolver.
fn entry_is_commonjs(pkg_root: &Path) -> bool {
    fn cjs_file(pkg_type: &str, rel: &str) -> bool {
        let rel = rel.trim_start_matches("./");
        if rel.ends_with(".cjs") {
            true
        } else if rel.ends_with(".mjs") || rel.ends_with(".json") {
            false
        } else {
            pkg_type != "module"
        }
    }
    fn cjs_target(pkg_type: &str, v: &Value) -> bool {
        match v {
            Value::String(s) => cjs_file(pkg_type, s),
            Value::Array(items) => !items.iter().any(|i| !cjs_target(pkg_type, i)),
            Value::Object(map) => {
                if ["import", "node"].iter().any(|c| map.contains_key(*c)) {
                    return false; // ESM by condition (dual-package)
                }
                if let Some(d) = map.get("default") {
                    return cjs_target(pkg_type, d);
                }
                map.contains_key("require")
            }
            _ => false,
        }
    }
    let Ok(raw) = fs::read(pkg_root.join("package.json")) else {
        return false;
    };
    let Ok(pkg) = serde_json::from_slice::<Value>(&raw) else {
        return false;
    };
    let pkg_type = pkg.get("type").and_then(Value::as_str).unwrap_or("");
    match pkg.get("exports") {
        Some(Value::String(s)) => cjs_file(pkg_type, s),
        Some(Value::Object(map)) => {
            let is_subpath = map.keys().any(|k| k == "." || k.starts_with("./"));
            if is_subpath {
                map.get(".").map(|v| cjs_target(pkg_type, v)).unwrap_or(false)
            } else {
                cjs_target(pkg_type, pkg.get("exports").unwrap())
            }
        }
        _ => {
            let main = pkg.get("main").and_then(Value::as_str).unwrap_or("index.js");
            cjs_file(pkg_type, main)
        }
    }
}

/// Default patch-spec dir: $INKA_PATCHES -> ./patches -> <dir of inka binary>/patches.
fn default_patches_base() -> PathBuf {
    if let Ok(p) = std::env::var("INKA_PATCHES") {
        return PathBuf::from(p);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let local = cwd.join("patches");
    if local.is_dir() {
        return local;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("patches");
            if p.is_dir() {
                return p;
            }
        }
    }
    local
}

/// Convert a CJS package (at scratch `nm/<name>`) in place using a known patch
/// spec if one exists. Returns Some(converted_kind) or errors on unpatched CJS.
fn convert_if_needed(nm: &Path, name: &str, version: &str) -> Result<Option<String>, String> {
    if !entry_is_commonjs(&nm.join(name)) {
        return Ok(None);
    }
    let spec = default_patches_base()
        .join(name)
        .join(version)
        .join("patch.json");
    if !spec.is_file() {
        return Err(format!(
            "'{name}@{version}' is CommonJS and has no patch spec at {};\n  add a patches/{name}/{version}/patch.json (see crates/inka-patcher) or keep the package in the default store",
            spec.display()
        ));
    }
    let bin = pkg::patcher_binary().map_err(|e| {
        format!(
            "conversion needs the patcher: {e}\n  (build crates/inka-patcher and keep it next to the inka binary)"
        )
    })?;
    let mut cmd = Command::new(&bin);
    cmd.arg("apply")
        .arg("--spec")
        .arg(&spec)
        .arg("--node-modules")
        .arg(nm);
    pkg::run_ok(&mut cmd, &format!("inka-patcher apply {}", spec.display()))?;
    let kind = fs::read_to_string(&spec)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "file-patch".to_string());
    Ok(Some(kind))
}

// ---- add -------------------------------------------------------------------

pub(crate) fn cmd_add(args: &[String]) {
    let mut force = false;
    let mut specs: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--force" | "-f" => force = true,
            "--help" | "-h" => {
                eprintln!("usage: inka add <pkg[@ver]> [--force]");
                std::process::exit(0);
            }
            other => specs.push(other.to_string()),
        }
    }
    if specs.is_empty() {
        fail("inka add needs a package name");
    }
    if specs.len() > 1 {
        fail("inka add takes one package at a time (for now)");
    }
    let spec = parse_add_spec(&specs[0]).unwrap_or_else(|e| fail(&e));
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

    // 1) dedupe: default store already satisfies -> skip (unless --force)
    if !force && store_satisfies(&store, &spec.name, spec.req.as_deref()) {
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
        .map(|(n, vers)| format!("{n} ({})", vers.iter().cloned().collect::<Vec<_>>().join(", ")))
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

    // 5) convert CJS leaves among the ones we will vendor (in scratch, pre-copy)
    let mut converted: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut todo = Vec::new();
    for (name, ver, _why) in &to_vendor {
        todo.push((name.clone(), ver.clone()));
    }
    todo.sort();
    for (name, ver) in todo {
        match convert_if_needed(&nm, &name, &ver) {
            Ok(Some(kind)) => {
                converted.entry(name.clone()).or_default().push(kind);
            }
            Ok(None) => {}
            Err(e) => {
                let _ = fs::remove_dir_all(&work);
                fail(&e);
            }
        }
    }

    // 6) place roots under vendored/
    let root = vendor_root();
    fs::create_dir_all(&root).unwrap_or_else(|e| fail(&format!("cannot create {}: {e}", root.display())));
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
        entry.converted = converted.get(name).cloned().unwrap_or_default();
    }
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
        println!(
            "  vendored {name}@{ver} ({why}{})",
            converted
                .get(name)
                .map(|c| format!(", converted {c:?}"))
                .unwrap_or_default()
        );
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
            let is_root = manifests_declares(&manifests, &name) || lock.entries[&name].why == "root";
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
fn package_requires(root: &Path, dep: &str) -> bool {
    let Ok(top) = fs::read_dir(root) else {
        return false;
    };
    for ent in top.flatten() {
        let dir = ent.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(raw) = fs::read(dir.join("package.json")) else { continue };
        let Ok(v) = serde_json::from_slice::<Value>(&raw) else { continue };
        let deps = v
            .get("dependencies")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if deps.contains_key(dep) {
            return true;
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
        let ignored = gi.lines().any(|l| l.trim().trim_end_matches('/') == VENDOR_DIR);
        println!("git posture: {} (vendored/ {})", if ignored { "ignore (dev)" } else { "commit (release)" }, if ignored { "ignored" } else { "not ignored" });
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
    let has = existing.lines().any(|l| l.trim().trim_end_matches('/') == VENDOR_DIR);
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
    let kept: Vec<&str> = existing.lines().filter(|l| l.trim().trim_end_matches('/') != VENDOR_DIR).collect();
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

// inka pkg: the vendored-package store.
//
//   inka pkg snapshot [--seed-manifest <file>] [--out <dir>]
//   inka pkg seed     [--from <dir-or-url>] [--store <dir>] [--insecure]
//   inka pkg list     [--store <dir>]
//
// The store is ONE shared, hoisted `node_modules` pool (a normal npm project
// layout), so independent packages can carry different versions of a shared
// dependency the same way Node does (hoisted copy + nested duplicates). jsr
// packages are served through jsr's npm-mirror identity (@jsr/scope__name).
//
// Layout:
//
//   <store>/                 ~/.inka-runtime/store  (or $INKA_STORE)
//     seed-manifest.json     record of installed top-levels + snapshot sha
//     node_modules/…         the whole resolved tree
//
// Distribution is a whole-store snapshot: `snapshot` npm-installs the seed set
// together (pre/postinstall already run there, before the tar is made) and
// packages the resolved node_modules as store.tar.gz. `seed` (and `inka
// install`, for a release's store/ payload) only downloads -> verifies ->
// replaces node_modules. Nothing installs or runs on the consumer machine.
//
// CommonJS packages that the engine cannot run are converted to engine-viable
// pure ESM at snapshot time, inside the scratch node_modules BEFORE the tar:
// repo-managed specs under `patches/<pkg>/<version>/patch.json` are applied by
// the sibling `inka-patcher` binary ($INKA_PATCHER or next to the inka binary).
// Discovery: --patches <dir> -> <dir of the seed manifest>/patches.
//
// The set of packages to seed comes from a user-editable seed-manifest.json
// (NOT hard-coded): { "seed": [ { "name", "version", "registry" } ] }.
// Discovery order: --seed-manifest -> $INKA_SEED_MANIFEST -> ./seed-manifest.json
// -> <dir of inka binary>/seed-manifest.json.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{fetch_with_sidecar, hex, runtime_dir};
use serde_json::Value;
use sha2::{Digest, Sha256};

const PKG_HELP: &str = "usage:\n  inka pkg snapshot [--seed-manifest <file>] [--patches <dir>] [--out <dir>]   build a whole-store snapshot tar (network)\n  inka pkg seed     [--from <dir-or-url>] [--store <dir>] [--insecure]   install a snapshot into the store\n  inka pkg list     [--store <dir>]";

const SNAPSHOT_TAR: &str = "store.tar.gz";
const STORE_MANIFEST: &str = "seed-manifest.json";

/// The user-curated, editable input manifest.
#[derive(serde::Serialize, serde::Deserialize)]
struct SeedManifest {
    seed: Vec<SeedSpec>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SeedSpec {
    name: String,
    #[serde(default)]
    registry: String, // "npm" (default) or "jsr"
    version: String,
}

/// The store/payload record (what got installed / what a snapshot contains).
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct SeedRecord {
    #[serde(default)]
    seeded: Vec<Installed>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    patched: Vec<PatchRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tar: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct PatchRecord {
    name: String,
    version: String,
    kind: String,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct Installed {
    name: String,
    version: String,
}

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1);
}

fn valid_version(s: &str) -> bool {
    let mut parts = s.split('.');
    let a = parts.next().and_then(|p| p.parse::<u64>().ok());
    let b = parts.next().and_then(|p| p.parse::<u64>().ok());
    let c = parts.next().and_then(|p| p.parse::<u64>().ok());
    a.is_some() && b.is_some() && c.is_some() && parts.next().is_none()
}

pub(crate) fn jsr_to_mirror(name: &str) -> Option<String> {
    // jsr:@scope/name -> npm mirror @jsr/scope__name
    let body = name.strip_prefix('@')?;
    let (scope, pkg) = body.split_once('/')?;
    if scope.is_empty() || pkg.is_empty() || pkg.contains('/') {
        return None;
    }
    Some(format!("@jsr/{scope}__{pkg}"))
}

/// Validate a spec and turn it into an npm install target for the combined run.
fn install_target(spec: &SeedSpec) -> Result<String, String> {
    if spec.name.is_empty() || spec.name.starts_with('/') || spec.name.ends_with('/') {
        return Err(format!("invalid package name '{}'", spec.name));
    }
    if !valid_version(&spec.version) {
        return Err(format!(
            "seed entry '{}' needs an exact x.y.z version (got '{}')",
            spec.name, spec.version
        ));
    }
    let reg = spec.registry.trim().to_ascii_lowercase();
    match reg.as_str() {
        "" | "npm" => Ok(format!("{}@{}", spec.name, spec.version)),
        "jsr" => {
            let mirror = jsr_to_mirror(&spec.name).ok_or_else(|| {
                format!(
                    "jsr seed entry '{}' must be a scoped name like @scope/name",
                    spec.name
                )
            })?;
            Ok(format!("{mirror}@{}", spec.version))
        }
        other => Err(format!("unknown registry '{}' (expected npm or jsr)", other)),
    }
}

pub(crate) fn store_default() -> PathBuf {
    if let Ok(s) = std::env::var("INKA_STORE") {
        return PathBuf::from(s);
    }
    runtime_dir(None).join("store")
}

pub(crate) fn run_ok(cmd: &mut Command, what: &str) -> Result<(), String> {
    let status = cmd
        .status()
        .map_err(|e| format!("failed to spawn {what}: {e}"))?;
    if !status.success() {
        return Err(format!("{what} exited with {status}"));
    }
    Ok(())
}

pub(crate) fn sha256_bytes(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Scan the top-level packages of a store root's node_modules.
fn scan_installed(store: &Path) -> Vec<Installed> {
    fn version_of(pkg_dir: &Path) -> String {
        let raw = match fs::read(pkg_dir.join("package.json")) {
            Ok(b) => b,
            Err(_) => return "?".into(),
        };
        serde_json::from_slice::<serde_json::Value>(&raw)
            .ok()
            .and_then(|v| v.get("version").and_then(serde_json::Value::as_str).map(str::to_string))
            .unwrap_or_else(|| "?".into())
    }
    let mut out = Vec::new();
    let nm = store.join("node_modules");
    let Ok(top) = fs::read_dir(&nm) else { return out };
    let mut entries: Vec<PathBuf> = top.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    entries.sort();
    for dir in entries {
        let name = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue; // .bin, .package-lock.json, …
        }
        if name.starts_with('@') {
            // scoped: @scope/<pkg>
            let Ok(sub) = fs::read_dir(&dir) else { continue };
            let mut subs: Vec<PathBuf> = sub.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
            subs.sort();
            for p in subs {
                let pkg = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
                out.push(Installed {
                    name: format!("{name}/{pkg}"),
                    version: version_of(&p),
                });
            }
        } else {
            out.push(Installed {
                name: name.clone(),
                version: version_of(&dir),
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn write_record(path: &Path, record: &SeedRecord) -> Result<(), String> {
    let json = serde_json::to_string_pretty(record).map_err(|e| format!("encode manifest: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &json).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("cannot finalize {}: {e}", path.display()))
}

// ---- seed-time package patching (Option C) ---------------------------------

fn patch_base(patches_flag: Option<&str>, seed_manifest: &Path) -> PathBuf {
    if let Some(p) = patches_flag {
        return PathBuf::from(p);
    }
    seed_manifest
        .parent()
        .map(|d| d.join("patches"))
        .unwrap_or_else(|| PathBuf::from("patches"))
}

/// All `patches/<pkg>/<version>/patch.json` specs under a base dir.
fn discover_patch_specs(base: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(versions) = fs::read_dir(base) else {
        return out;
    };
    for pkg in versions.flatten() {
        let pkg_dir = pkg.path();
        if !pkg_dir.is_dir() {
            continue;
        }
        let Ok(ver_dirs) = fs::read_dir(&pkg_dir) else { continue };
        for v in ver_dirs.flatten() {
            let spec = v.path().join("patch.json");
            if spec.is_file() {
                out.push(spec);
            }
        }
    }
    out.sort();
    out
}

pub(crate) fn patcher_binary() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("INKA_PATCHER") {
        if Path::new(&p).is_file() {
            return Ok(PathBuf::from(p));
        }
        return Err(format!("INKA_PATCHER points to a missing file: {p}"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("inka-patcher");
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    Err(
        "patch specs exist but no inka-patcher binary found; build it with the big-disk \
         cargo home/target (cargo build --release -p inka-patcher) and keep it next to \
         this inka binary (or set INKA_PATCHER)"
            .into(),
    )
}

/// Apply repo-managed patch specs to the scratch node_modules (in place, pre-tar).
/// Returns the list of applied patches for the record. No specs -> no-op.
fn apply_patches(
    node_modules: &Path,
    seed_manifest: &Path,
    patches_flag: Option<&str>,
) -> Result<Vec<PatchRecord>, String> {
    let base = patch_base(patches_flag, seed_manifest);
    let specs = discover_patch_specs(&base);
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    let bin = patcher_binary()?;
    let mut record = Vec::new();
    for spec in specs {
        let name = spec
            .parent()
            .and_then(|v| v.parent())
            .and_then(|p| p.file_name())
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let mut cmd = Command::new(&bin);
        cmd.arg("apply")
            .arg("--spec")
            .arg(&spec)
            .arg("--node-modules")
            .arg(node_modules);
        run_ok(&mut cmd, &format!("inka-patcher apply {}", spec.display()))?;
        let raw = fs::read(&spec).map_err(|e| format!("re-read {}: {e}", spec.display()))?;
        let v: serde_json::Value =
            serde_json::from_slice(&raw).map_err(|e| format!("parse {}: {e}", spec.display()))?;
        record.push(PatchRecord {
            name,
            version: v
                .get("version")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            kind: v
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
        });
    }
    Ok(record)
}

/// Atomically replace the store's node_modules with a freshly extracted tree.
fn swap_node_modules(store: &Path, tar_bytes: &[u8]) -> Result<(), String> {
    fs::create_dir_all(store).map_err(|e| format!("cannot create {}: {e}", store.display()))?;
    // Drop the old pool (and any legacy per-package layout) before extracting.
    for stale in ["node_modules", "packages"] {
        let p = store.join(stale);
        if p.exists() {
            fs::remove_dir_all(&p).map_err(|e| format!("cannot remove {}: {e}", p.display()))?;
        }
    }
    let tmp = store.join(format!(".store.tmp{}", std::process::id()));
    fs::write(&tmp, tar_bytes).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    let mut cmd = Command::new("tar");
    cmd.args(["-xzf"]).arg(&tmp).arg("-C").arg(store);
    let res = run_ok(&mut cmd, "tar extract");
    let _ = fs::remove_file(&tmp);
    res
}

// ---- manifest discovery -----------------------------------------------------

fn manifest_from_flag(flag: Option<&str>) -> Result<PathBuf, String> {
    if let Some(f) = flag {
        let p = PathBuf::from(f);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!("seed manifest not found: {}", p.display()));
    }
    if let Some(e) = std::env::var_os("INKA_SEED_MANIFEST") {
        let p = PathBuf::from(e);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!(
            "INKA_SEED_MANIFEST points to a missing file: {}",
            p.display()
        ));
    }
    if let Ok(cwd) = std::env::current_dir() {
        let p = cwd.join(STORE_MANIFEST);
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join(STORE_MANIFEST);
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    Err(
        "no seed manifest found; pass --seed-manifest <file>, set INKA_SEED_MANIFEST, or put a \
         seed-manifest.json in the current directory / next to the inka binary"
            .into(),
    )
}

fn load_seed_manifest(path: &Path) -> Result<SeedManifest, String> {
    let raw = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let m: SeedManifest =
        serde_json::from_slice(&raw).map_err(|e| format!("invalid seed manifest {}: {e}", path.display()))?;
    if m.seed.is_empty() {
        return Err(format!("seed manifest {} lists no packages", path.display()));
    }
    Ok(m)
}

// ---- seed-time CJS lint (heads-up only) -----------------------------------
//
// Warn about installed packages whose import-reachable entry is CommonJS and has
// no patch record. Mirrors the resolver's classification (import/node conditions
// => ESM by context; otherwise .cjs / non-"module" .js => CJS) as a heuristic,
// not a full resolve. Not fatal: the resolver rejects such a package cleanly at
// run time if it is actually imported.

fn classify_cjs_file(pkg_type: &str, rel: &str) -> bool {
    let rel = rel.trim_start_matches("./");
    if rel.ends_with(".cjs") {
        return true;
    }
    if rel.ends_with(".mjs") || rel.ends_with(".json") || rel.ends_with(".node") {
        return false;
    }
    pkg_type != "module"
}

fn exports_dot_is_cjs(pkg_type: &str, v: &Value) -> bool {
    match v {
        Value::String(s) => classify_cjs_file(pkg_type, s),
        Value::Array(items) => {
            // Arrays are rare for ".": flag only if no usable branch is ESM.
            !items.iter().any(|i| !exports_dot_is_cjs(pkg_type, i))
        }
        Value::Object(map) => {
            for cond in ["import", "node"] {
                if map.contains_key(cond) {
                    return false; // ESM by condition (dual-package pattern)
                }
            }
            if let Some(d) = map.get("default") {
                return exports_dot_is_cjs(pkg_type, d);
            }
            map.contains_key("require")
        }
        _ => false,
    }
}

fn package_entry_is_cjs(pkg: &Value) -> bool {
    let pkg_type = pkg.get("type").and_then(Value::as_str).unwrap_or("");
    match pkg.get("exports") {
        Some(Value::String(s)) => classify_cjs_file(pkg_type, s),
        Some(Value::Object(map)) => {
            let is_subpath_map = map.keys().any(|k| k == "." || k.starts_with("./"));
            if is_subpath_map {
                match map.get(".") {
                    Some(dot) => exports_dot_is_cjs(pkg_type, dot),
                    None => false, // no "." export: root not importable
                }
            } else {
                exports_dot_is_cjs(pkg_type, pkg.get("exports").unwrap())
            }
        }
        _ => {
            let main = pkg.get("main").and_then(Value::as_str).unwrap_or("index.js");
            classify_cjs_file(pkg_type, main)
        }
    }
}

/// Collect the top-level (hoisted) packages of a node_modules tree. Unpatched CJS
/// leaves that the ESM graph actually reaches are hoisted here (npm dedupes), so
/// top-level-only keeps the lint signal high while skipping nested optional
/// natives and nested dupes that aren't importable as ESM roots.
fn top_level_packages(nm: &Path) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let Ok(top) = fs::read_dir(nm) else { return out };
    let mut entries: Vec<PathBuf> = top.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    entries.sort();
    for dir in entries {
        let name = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if name.starts_with('@') {
            let Ok(sub) = fs::read_dir(&dir) else { continue };
            let mut subs: Vec<PathBuf> = sub.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
            subs.sort();
            for p in subs {
                let pkg = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
                let display = format!("{name}/{pkg}");
                if let Ok(raw) = fs::read(p.join("package.json")) {
                    if let Ok(v) = serde_json::from_slice::<Value>(&raw) {
                        out.push((display, v));
                    }
                }
            }
        } else if let Ok(raw) = fs::read(dir.join("package.json")) {
            if let Ok(v) = serde_json::from_slice::<Value>(&raw) {
                out.push((name, v));
            }
        }
    }
    out
}

fn lint_unpatched_cjs(node_modules: &Path, patched: &[PatchRecord], seeds: &[SeedSpec]) {
    // Only direct seeds are the user's responsibility: warn when a seeded package
    // resolves CJS and has no patch spec. Transitive CJS leaves are handled by the
    // repo's curated patch specs (discovered empirically); optional natives /
    // unreachable CJS would otherwise flood every snapshot with noise.
    let mut seed_names = std::collections::HashSet::new();
    for s in seeds {
        let reg = s.registry.trim().to_ascii_lowercase();
        if reg == "jsr" {
            if let Some(m) = jsr_to_mirror(&s.name) {
                seed_names.insert(m);
            }
        } else {
            seed_names.insert(s.name.clone());
        }
    }
    let patched_keys: Vec<String> = patched
        .iter()
        .map(|p| format!("{}@{}", p.name, p.version))
        .collect();
    for (name, pkg) in top_level_packages(node_modules) {
        if !seed_names.contains(&name) || !package_entry_is_cjs(&pkg) {
            continue;
        }
        let version = pkg.get("version").and_then(Value::as_str).unwrap_or("?");
        let key = format!("{name}@{version}");
        if !patched_keys.contains(&key) {
            eprintln!(
                "[inka] pkg snapshot: warning: seeded package {key} has a CommonJS entry and \
                 no patches/ spec; it will fail cleanly at run time if imported — add a patch \
                 spec under patches/{} or exclude it",
                pkg.get("name").and_then(Value::as_str).unwrap_or(&name)
            );
        }
    }
}

// ---- snapshot ---------------------------------------------------------------

fn cmd_snapshot(args: &[String]) {
    let mut seed_manifest: Option<String> = None;
    let mut patches: Option<String> = None;
    let mut out = PathBuf::from(".");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--seed-manifest" => {
                seed_manifest = Some(it.next().unwrap_or_else(|| fail("--seed-manifest needs a file")).clone())
            }
            "--patches" => {
                patches = Some(it.next().unwrap_or_else(|| fail("--patches needs a dir")).clone())
            }
            "--out" => out = PathBuf::from(it.next().unwrap_or_else(|| fail("--out needs a dir"))),
            "--help" | "-h" => {
                eprintln!("{PKG_HELP}");
                std::process::exit(0);
            }
            other => {
                eprintln!("error: unknown `inka pkg snapshot` argument '{other}'");
                std::process::exit(2);
            }
        }
    }
    let manifest_path = manifest_from_flag(seed_manifest.as_deref()).unwrap_or_else(|e| fail(&e));
    let manifest = load_seed_manifest(&manifest_path).unwrap_or_else(|e| fail(&e));

    let targets: Vec<String> = manifest
        .seed
        .iter()
        .map(install_target)
        .collect::<Result<_, _>>()
        .unwrap_or_else(|e| fail(&e));

    let work = std::env::temp_dir().join(format!("inka-store-snap-{}", std::process::id()));
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work).unwrap_or_else(|e| fail(&format!("cannot create workdir: {e}")));

    // 1) resolve the whole set together with a package manager (network allowed
    //    here only). pre/postinstall run now, before the tar is made.
    fs::write(work.join(".npmrc"), "@jsr:registry=https://npm.jsr.io\n")
        .unwrap_or_else(|e| fail(&format!("cannot write .npmrc: {e}")));
    println!(
        "[inka] pkg snapshot: resolving {} package(s) from {} (network)…",
        targets.len(),
        manifest_path.display()
    );
    let mut cmd = Command::new("npm");
    cmd.current_dir(&work)
        .args(["install", "--no-save", "--omit=dev"])
        .args(&targets);
    if let Err(e) = run_ok(&mut cmd, "npm install") {
        let _ = fs::remove_dir_all(&work);
        fail(&e);
    }
    if !work.join("node_modules").is_dir() {
        let _ = fs::remove_dir_all(&work);
        fail("npm install did not produce a node_modules directory");
    }

    // 1b) apply repo-managed CommonJS->ESM patches inside the scratch pool
    //     (before the tar), so the snapshot ships engine-viable packages.
    let patched =
        apply_patches(&work.join("node_modules"), &manifest_path, patches.as_deref())
            .unwrap_or_else(|e| {
                let _ = fs::remove_dir_all(&work);
                fail(&e);
            });
    for p in &patched {
        println!(
            "[inka] pkg snapshot: patched {}@{} ({})",
            p.name, p.version, p.kind
        );
    }
    lint_unpatched_cjs(&work.join("node_modules"), &patched, &manifest.seed);

    // 2) package the resolved pool as a whole-store snapshot.
    if !out.is_absolute() {
        out = std::env::current_dir().unwrap_or_default().join(out);
    }
    fs::create_dir_all(&out).unwrap_or_else(|e| fail(&format!("cannot create {}: {e}", out.display())));
    let tar_file = out.join(SNAPSHOT_TAR);
    let tar_tmp = out.join(format!(".{SNAPSHOT_TAR}.tmp{}", std::process::id()));
    let _ = fs::remove_file(&tar_tmp);
    let mut cmd = Command::new("tar");
    cmd.current_dir(&work).args(["-czf"]).arg(&tar_tmp).arg("node_modules");
    if let Err(e) = run_ok(&mut cmd, "tar") {
        let _ = fs::remove_dir_all(&work);
        fail(&e);
    }
    fs::rename(&tar_tmp, &tar_file).unwrap_or_else(|e| {
        let _ = fs::remove_dir_all(&work);
        fail(&format!("cannot finalize {}: {e}", tar_file.display()))
    });
    let tar_bytes = fs::read(&tar_file).unwrap_or_default();
    let sha = sha256_bytes(&tar_bytes);
    fs::write(out.join(format!("{SNAPSHOT_TAR}.sha256")), format!("{sha}\n"))
        .unwrap_or_else(|e| fail(&format!("cannot write checksum sidecar: {e}")));

    let record = SeedRecord {
        seeded: scan_installed(&work),
        patched,
        tar: Some(SNAPSHOT_TAR.to_string()),
        sha256: Some(sha.clone()),
    };
    write_record(&out.join(STORE_MANIFEST), &record).unwrap_or_else(|e| fail(&e));

    let _ = fs::remove_dir_all(&work);
    println!(
        "[inka] pkg snapshot: wrote {} ({} bytes, sha256 {}), manifest {}",
        tar_file.display(),
        tar_bytes.len(),
        &sha[..12],
        out.join(STORE_MANIFEST).display()
    );
}

// ---- seed -------------------------------------------------------------------

fn fetch_record_and_tar(base: &str, rel_dir: &str) -> Result<(SeedRecord, Vec<u8>), String> {
    let rel_manifest = if rel_dir.is_empty() {
        STORE_MANIFEST.to_string()
    } else {
        format!("{rel_dir}/{STORE_MANIFEST}")
    };
    let (mbytes, _) = fetch_with_sidecar(base, &rel_manifest)
        .map_err(|e| format!("no seed manifest at {rel_manifest}: {e}"))?;
    let record: SeedRecord = serde_json::from_slice(&mbytes)
        .map_err(|e| format!("invalid seed manifest at {rel_manifest}: {e}"))?;
    let tar_name = record.tar.clone().unwrap_or_else(|| SNAPSHOT_TAR.to_string());
    let rel_tar = if rel_dir.is_empty() {
        tar_name.clone()
    } else {
        format!("{rel_dir}/{tar_name}")
    };
    let (tbytes, sidecar) = fetch_with_sidecar(base, &rel_tar)
        .map_err(|e| format!("failed to fetch {rel_tar}: {e}"))?;
    let actual = sha256_bytes(&tbytes);
    let expected = record
        .sha256
        .clone()
        .or_else(|| {
            sidecar.map(|s| {
                s.split_whitespace()
                    .next()
                    .unwrap_or(&s)
                    .trim()
                    .to_ascii_lowercase()
                    .to_string()
            })
        });
    if let Some(exp) = &expected {
        if exp != &actual {
            return Err(format!(
                "checksum mismatch for {rel_tar}: expected {exp}, actual {actual}"
            ));
        }
    }
    Ok((record, tbytes))
}

fn cmd_seed(args: &[String]) {
    let mut from: Option<String> = None;
    let mut store = store_default();
    let mut insecure = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--from" => from = Some(it.next().unwrap_or_else(|| fail("--from needs a value")).clone()),
            "--store" => store = PathBuf::from(it.next().unwrap_or_else(|| fail("--store needs a dir"))),
            "--insecure" => insecure = true,
            "--help" | "-h" => {
                eprintln!("{PKG_HELP}");
                std::process::exit(0);
            }
            other => {
                eprintln!("error: unknown `inka pkg seed` argument '{other}'");
                std::process::exit(2);
            }
        }
    }
    let Some(base) = from.or_else(|| std::env::var("INKA_PKG_SOURCE").ok()) else {
        eprintln!("error: `inka pkg seed` needs a source (use --from <dir-or-url> or INKA_PKG_SOURCE)");
        std::process::exit(2);
    };

    let (record, tbytes) = match fetch_record_and_tar(&base, "") {
        Ok(x) => x,
        Err(e) if !insecure => fail(&e),
        Err(e) => {
            // --insecure: tolerate a missing manifest/sha but still need a tar
            let rel = SNAPSHOT_TAR.to_string();
            let (tb, _) = match fetch_with_sidecar(&base, &rel) {
                Ok(x) => x,
                Err(e2) => fail(&format!("{e}; also failed to fetch {rel}: {e2}")),
            };
            (
                SeedRecord {
                    seeded: Vec::new(),
                    patched: Vec::new(),
                    tar: None,
                    sha256: None,
                },
                tb,
            )
        }
    };

    if let Err(e) = swap_node_modules(&store, &tbytes) {
        fail(&e);
    }
    let seeded = scan_installed(&store);
    let out_record = SeedRecord {
        seeded: seeded.clone(),
        patched: record.patched.clone(),
        tar: record.tar,
        sha256: record.sha256,
    };
    write_record(&store.join(STORE_MANIFEST), &out_record).unwrap_or_else(|e| fail(&e));
    println!(
        "[inka] pkg seed: store updated at {} ({} packages)",
        store.display(),
        seeded.len()
    );
}

// ---- install payload --------------------------------------------------------

/// Seeds the store from a runtime release's vendored payload under
/// `<source>/store/`. Returns `Ok(None)` when the release has no store payload.
pub(crate) fn seed_release_store(source: &str, store: &Path) -> Result<Option<usize>, String> {
    let (record, tbytes) = match fetch_record_and_tar(source, "store") {
        Ok(x) => x,
        Err(_) => return Ok(None), // older release / no vendored payload
    };
    swap_node_modules(store, &tbytes)?;
    let seeded = scan_installed(store);
    let out_record = SeedRecord {
        seeded: seeded.clone(),
        patched: record.patched.clone(),
        tar: record.tar,
        sha256: record.sha256,
    };
    write_record(&store.join(STORE_MANIFEST), &out_record)?;
    Ok(Some(seeded.len()))
}

// ---- list -------------------------------------------------------------------

fn cmd_list(args: &[String]) {
    let mut store = store_default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--store" => store = PathBuf::from(it.next().unwrap_or_else(|| fail("--store needs a dir"))),
            "--help" | "-h" => {
                eprintln!("{PKG_HELP}");
                std::process::exit(0);
            }
            other => {
                eprintln!("error: unknown `inka pkg list` argument '{other}'");
                std::process::exit(2);
            }
        }
    }
    let rows = scan_installed(&store);
    if rows.is_empty() {
        println!("(no packages in store {})", store.display());
        return;
    }
    for r in rows {
        println!("{}@{}", r.name, r.version);
    }
}

// ---- dispatch ---------------------------------------------------------------

pub(crate) fn cmd_pkg(args: &[String]) {
    let Some(cmd) = args.first() else {
        eprintln!("{PKG_HELP}");
        std::process::exit(2);
    };
    let rest = &args[1..];
    match cmd.as_str() {
        "snapshot" | "tar" => cmd_snapshot(rest),
        "seed" => cmd_seed(rest),
        "list" => cmd_list(rest),
        "--help" | "-h" => {
            eprintln!("{PKG_HELP}");
            std::process::exit(0);
        }
        other => {
            eprintln!("error: unknown `inka pkg` subcommand '{other}'");
            eprintln!("{PKG_HELP}");
            std::process::exit(2);
        }
    }
}

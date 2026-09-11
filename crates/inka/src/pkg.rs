// inka store: the shared vendored-package store + release snapshot builder.
//
// The user-facing entry points are `inka update` (syncs the store with the
// newest release) and `inka add`/`install` (project vendoring). The only
// non-user-facing entry point here is the release-time snapshot builder,
// invoked by CI as `inka internal snapshot-store`.
//
// The store is ONE shared, hoisted `node_modules` pool (a normal npm project
// layout), so independent packages can carry different versions of a shared
// dependency the same way Node does (hoisted copy + nested duplicates). jsr
// packages are served through jsr's npm-mirror identity (@jsr/scope__name).
//
// Layout:
//
//   <store>/                 ~/.local/share/inka/store  (or $INKA_STORE)
//     seed-manifest.json     record of installed top-levels + snapshot sha
//     node_modules/…         the whole resolved tree
//
// Distribution is a whole-store snapshot: the snapshot builder npm-installs the
// seed set together (pre/postinstall already run there, before the tar is made)
// and packages the resolved node_modules as store.tar.gz. `inka update` only
// downloads -> verifies -> replaces node_modules. Nothing installs or runs on
// the consumer machine.
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

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{fetch_with_sidecar, hex};
use serde_json::Value;
use sha2::{Digest, Sha256};

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
pub(crate) struct SeedRecord {
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
    let tmp = path.with_extension(format!("json.tmp{}", std::process::id()));
    fs::write(&tmp, &json).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("cannot finalize {}: {e}", path.display()))
}

// ---- seed-time package patching (Option C) ---------------------------------
//
// Multiple specs for the same package (one per version) may coexist; each
// installed occurrence is patched with the spec whose exact version matches it
// (see crate::patches). Specs that match no installed package are skipped with
// a note.

/// Apply repo-managed patch specs to the scratch node_modules (in place,
/// pre-tar), matching each installed occurrence's exact version. Returns the
/// list of applied patches (deduped by package@version) for the record.
fn apply_patches(
    node_modules: &Path,
    seed_manifest: &Path,
    patches_flag: Option<&str>,
) -> Result<Vec<PatchRecord>, String> {
    let base = crate::patches::release_base(patches_flag, seed_manifest);
    let (applies, skipped) = crate::patches::plan_snapshot(&base, node_modules)?;
    if applies.is_empty() && skipped.is_empty() {
        return Ok(Vec::new());
    }
    // Fail fast (before any mutation) when patches must be applied but the
    // patcher binary is missing.
    if !applies.is_empty() {
        crate::patches::patcher_binary()?;
    }

    let mut record = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for (spec, nm) in &applies {
        crate::patches::invoke(&spec.path, nm)?;
        if seen.insert((spec.package.clone(), spec.version.clone())) {
            record.push(PatchRecord {
                name: spec.package.clone(),
                version: spec.version.clone(),
                kind: spec.kind.clone(),
            });
        }
    }
    for spec in &skipped {
        println!(
            "[inka] snapshot-store: note: {}@{} patch not applied (not installed)",
            spec.package, spec.version
        );
    }
    Ok(record)
}

/// Atomically replace the store's node_modules with a freshly extracted tree.
/// Replace the store's `node_modules` with the snapshot, atomically: extract
/// into a staging dir first, move the live pool aside, move the new one in, then
/// clean up. A failed extract (or activation) leaves the previous store intact.
fn swap_node_modules(store: &Path, tar_bytes: &[u8]) -> Result<(), String> {
    fs::create_dir_all(store).map_err(|e| format!("cannot create {}: {e}", store.display()))?;
    let pid = std::process::id();

    let staging = store.join(format!(".store.stage{pid}"));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| format!("cannot create {}: {e}", staging.display()))?;

    let tar_path = staging.join(".store.tar");
    if let Err(e) = fs::write(&tar_path, tar_bytes) {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!("cannot write {}: {e}", tar_path.display()));
    }
    let mut cmd = Command::new("tar");
    cmd.args(["-xzf"]).arg(&tar_path).arg("-C").arg(&staging);
    let extract = run_ok(&mut cmd, "tar extract");
    let _ = fs::remove_file(&tar_path);
    if let Err(e) = extract {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }
    let new_nm = staging.join("node_modules");
    if !new_nm.is_dir() {
        let _ = fs::remove_dir_all(&staging);
        return Err("store snapshot has no node_modules/ directory".into());
    }

    let live = store.join("node_modules");
    let old = store.join(format!(".node_modules.old{pid}"));
    let _ = fs::remove_dir_all(&old);
    let had_live = live.exists();
    if had_live {
        fs::rename(&live, &old).map_err(|e| {
            let _ = fs::remove_dir_all(&staging);
            format!("cannot move the current store aside: {e}")
        })?;
    }
    if let Err(e) = fs::rename(&new_nm, &live) {
        if had_live {
            let _ = fs::rename(&old, &live); // restore the previous pool
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(format!("cannot activate the new store: {e}"));
    }
    let _ = fs::remove_dir_all(&old);
    let _ = fs::remove_dir_all(&staging);
    // Drop the legacy per-package layout once the new pool is live.
    let packages = store.join("packages");
    if packages.exists() {
        let _ = fs::remove_dir_all(&packages);
    }
    Ok(())
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
                "[inka] snapshot-store: warning: seeded package {key} has a CommonJS entry and \
                 no patches/ spec; it will fail cleanly at run time if imported — add a patch \
                 spec under patches/{} or exclude it",
                pkg.get("name").and_then(Value::as_str).unwrap_or(&name)
            );
        }
    }
}

// ---- snapshot ---------------------------------------------------------------

pub(crate) fn cmd_snapshot_store(args: &[String]) {
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
                eprintln!(
                    "usage: inka internal snapshot-store [--seed-manifest <file>] \
                     [--patches <dir>] [--out <dir>]"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("error: unknown `snapshot-store` argument '{other}'");
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
        "[inka] snapshot-store: resolving {} package(s) from {} (network)…",
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
            "[inka] snapshot-store: patched {}@{} ({})",
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
        "[inka] snapshot-store: wrote {} ({} bytes, sha256 {}), manifest {}",
        tar_file.display(),
        tar_bytes.len(),
        &sha[..12],
        out.join(STORE_MANIFEST).display()
    );
}

// ---- store sync for `inka update` -------------------------------------------

/// Fetch just the store record (`seed-manifest.json`) from a release base,
/// trying flat GitHub assets first, then a `store/` subdir layout.
pub(crate) fn fetch_store_record(base: &str) -> Result<SeedRecord, String> {
    let flat = fetch_with_sidecar(base, STORE_MANIFEST);
    let (mbytes, _) = match flat {
        Ok(x) => x,
        Err(_) => fetch_with_sidecar(base, &format!("store/{STORE_MANIFEST}"))
            .map_err(|e| format!("no store seed manifest at {base}: {e}"))?,
    };
    serde_json::from_slice(&mbytes).map_err(|e| format!("invalid store seed manifest: {e}"))
}

/// The snapshot identity recorded in a store record (empty when absent).
pub(crate) fn record_sha(record: &SeedRecord) -> String {
    record.sha256.clone().unwrap_or_default()
}

/// The snapshot identity currently recorded in `store` (empty when none).
pub(crate) fn store_record_sha(store: &Path) -> String {
    fs::read(store.join(STORE_MANIFEST))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.get("sha256").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default()
}

/// Fetch the snapshot tar named by `record` (flat first, then `store/` subdir),
/// verifying against the record's `sha256` or the `.sha256` sidecar.
pub(crate) fn fetch_store_tar(base: &str, record: &SeedRecord) -> Result<Vec<u8>, String> {
    let tar_name = record.tar.clone().unwrap_or_else(|| SNAPSHOT_TAR.to_string());
    let (tbytes, sidecar) = match fetch_with_sidecar(base, &tar_name) {
        Ok(x) => x,
        Err(_) => fetch_with_sidecar(base, &format!("store/{tar_name}"))
            .map_err(|e| format!("failed to fetch {tar_name}: {e}"))?,
    };
    let actual = sha256_bytes(&tbytes);
    let expected = record.sha256.clone().or_else(|| {
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
                "checksum mismatch for {tar_name}: expected {exp}, actual {actual}"
            ));
        }
    }
    Ok(tbytes)
}

/// Apply a fetched snapshot to `store` (replace `node_modules`, write record).
pub(crate) fn apply_store_record(
    store: &Path,
    record: &SeedRecord,
    tbytes: &[u8],
) -> Result<usize, String> {
    swap_node_modules(store, tbytes)?;
    let seeded = scan_installed(store);
    let out_record = SeedRecord {
        seeded: seeded.clone(),
        patched: record.patched.clone(),
        tar: record.tar.clone(),
        sha256: record.sha256.clone(),
    };
    write_record(&store.join(STORE_MANIFEST), &out_record)?;
    Ok(seeded.len())
}

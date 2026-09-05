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
// The set of packages to seed comes from a user-editable seed-manifest.json
// (NOT hard-coded): { "seed": [ { "name", "version", "registry" } ] }.
// Discovery order: --seed-manifest -> $INKA_SEED_MANIFEST -> ./seed-manifest.json
// -> <dir of inka binary>/seed-manifest.json.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{fetch_with_sidecar, hex, runtime_dir};
use sha2::{Digest, Sha256};

const PKG_HELP: &str = "usage:\n  inka pkg snapshot [--seed-manifest <file>] [--out <dir>]   build a whole-store snapshot tar (network)\n  inka pkg seed     [--from <dir-or-url>] [--store <dir>] [--insecure]   install a snapshot into the store\n  inka pkg list     [--store <dir>]";

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
    #[serde(skip_serializing_if = "Option::is_none")]
    tar: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
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

fn jsr_to_mirror(name: &str) -> Option<String> {
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

fn store_default() -> PathBuf {
    if let Ok(s) = std::env::var("INKA_STORE") {
        return PathBuf::from(s);
    }
    runtime_dir(None).join("store")
}

fn run_ok(cmd: &mut Command, what: &str) -> Result<(), String> {
    let status = cmd
        .status()
        .map_err(|e| format!("failed to spawn {what}: {e}"))?;
    if !status.success() {
        return Err(format!("{what} exited with {status}"));
    }
    Ok(())
}

fn sha256_bytes(bytes: &[u8]) -> String {
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

// ---- snapshot ---------------------------------------------------------------

fn cmd_snapshot(args: &[String]) {
    let mut seed_manifest: Option<String> = None;
    let mut out = PathBuf::from(".");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--seed-manifest" => {
                seed_manifest = Some(it.next().unwrap_or_else(|| fail("--seed-manifest needs a file")).clone())
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

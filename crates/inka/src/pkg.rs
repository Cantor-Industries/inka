// inka pkg: the vendored-package store.
//
//   inka pkg tar  [--out <dir>] <spec>...      build self-contained closure tars
//   inka pkg seed [--from <dir-or-url>] [--store <dir>] [--insecure] [<spec>...]
//   inka pkg list [--store <dir>]
//
// A "package" is stored as a self-contained closure: one directory per
// installed version containing its own `node_modules` tree, produced by a real
// package manager at tar-build time (pre/postinstall scripts already run).
// Seeding is therefore just fetch -> verify -> extract; nothing runs, and no
// dependency resolution happens on the consumer machine.
//
// Layout (mirrored by crates/inka-runtime's PkgLoader):
//
//   <store>/
//     seed-manifest.json
//     packages/<npm-name>/<version>/node_modules/<npm-name>/...
//
// The only networked operations are `inka pkg tar` (release/dev) and
// `inka pkg seed` (fetching published tars); the runtime itself never fetches.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{fetch_with_sidecar, hex, runtime_dir};
use sha2::{Digest, Sha256};

const PKG_HELP: &str = "usage:\n  inka pkg tar  [--out <dir>] <spec>...      build self-contained closure tars\n  inka pkg seed [--from <dir-or-url>] [--store <dir>] [--insecure] [<spec>...]\n  inka pkg list [--store <dir>]";

/// The curated zero-install set shipped with a runtime release. Each spec is a
/// `npm:name@version` or `jsr:@scope/name@version` import the runtime resolves
/// from the store. Bump versions here and re-run `inka pkg tar` to republish.
const CURATED_SPECS: [&str; 2] = ["zod@3.23.0", "jsr:@std/assert@1.0.0"];

const SEED_MANIFEST: &str = "seed-manifest.json";

#[derive(Clone)]
struct CliSpec {
    /// npm package name (jsr specs already mapped to `@jsr/scope__name`).
    name: String,
    /// exact x.y.z version.
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

/// Parse `npm:name@version`, `jsr:@scope/name@version`, or bare `name@version`
/// into an npm identity + exact version. jsr specs map to their npm-compat
/// mirror identity (`jsr:@scope/name` -> `@jsr/scope__name`).
fn parse_cli_spec(spec: &str) -> Result<CliSpec, String> {
    let body = if let Some(rest) = spec.strip_prefix("npm:") {
        rest.to_string()
    } else if let Some(rest) = spec.strip_prefix("jsr:") {
        let rest = rest.trim();
        let (scope, after) = rest
            .split_once('/')
            .ok_or_else(|| format!("invalid jsr spec '{spec}'"))?;
        let scope = scope.strip_prefix('@').unwrap_or(scope);
        let (name, tail) = match after.find('@') {
            Some(i) => (&after[..i], &after[i..]),
            None => return Err(format!("jsr spec '{spec}' needs an exact version")),
        };
        format!("@jsr/{scope}__{name}{tail}")
    } else {
        spec.to_string()
    };

    let (name, ver) = if let Some(body) = body.strip_prefix('@') {
        // scoped: @scope/name@version
        let (scope, rest) = body
            .split_once('/')
            .ok_or_else(|| format!("malformed scoped spec '{spec}'"))?;
        let (nm, tail) = rest
            .split_once('@')
            .ok_or_else(|| format!("spec '{spec}' needs an exact version"))?;
        (format!("@{scope}/{nm}"), tail)
    } else {
        let (nm, tail) = body
            .rsplit_once('@')
            .ok_or_else(|| format!("spec '{spec}' needs an exact version"))?;
        (nm.to_string(), tail)
    };

    if ver.is_empty() || ver.contains('/') || !valid_version(ver) {
        return Err(format!("spec '{spec}' needs an exact x.y.z version"));
    }
    if name.is_empty() || name.starts_with('/') || name.ends_with('/') {
        return Err(format!("invalid package name in '{spec}'"));
    }
    Ok(CliSpec {
        name,
        version: ver.to_string(),
    })
}

/// Filename-safe identity for a package (scoped `/` becomes `+`).
fn file_base(spec: &CliSpec) -> String {
    format!("{}@{}.tar.gz", spec.name.replace('/', "+"), spec.version)
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

fn copy_dir(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst)
        .map_err(|e| format!("cannot create {}: {e}", dst.display()))?;
    for ent in fs::read_dir(src)
        .map_err(|e| format!("cannot read {}: {e}", src.display()))?
        .flatten()
    {
        let from = ent.path();
        let to = dst.join(ent.file_name());
        if ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            copy_dir(&from, &to)?;
        } else {
            fs::copy(&from, &to)
                .map_err(|e| format!("cannot copy {} -> {}: {e}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

// ---- tar --------------------------------------------------------------------

fn cmd_tar(args: &[String]) {
    let mut out = PathBuf::from(".");
    let mut specs = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => out = PathBuf::from(it.next().unwrap_or_else(|| fail("--out needs a dir"))),
            "--help" | "-h" => {
                eprintln!("{PKG_HELP}");
                std::process::exit(0);
            }
            _ => specs.push(a.clone()),
        }
    }
    if specs.is_empty() {
        eprintln!("error: `inka pkg tar` needs at least one spec (e.g. zod@3.23.0)");
        std::process::exit(2);
    }
    if !out.is_absolute() {
        out = std::env::current_dir().unwrap_or_default().join(out);
    }
    fs::create_dir_all(&out).unwrap_or_else(|e| fail(&format!("cannot create {}: {e}", out.display())));

    for spec in &specs {
        let parsed = parse_cli_spec(spec).unwrap_or_else(|e| fail(&e));
        let work = std::env::temp_dir().join(format!(
            "inka-pkg-tar-{}-{}",
            std::process::id(),
            file_base(&parsed)
        ));
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work).unwrap_or_else(|e| fail(&format!("cannot create workdir: {e}")));
        let staging = work.join("staging");

        // 1) resolve a real, self-contained node_modules closure with a package
        //    manager (network allowed here only). pre/postinstall scripts run
        //    during this step, before the tar is made.
        fs::write(work.join(".npmrc"), "@jsr:registry=https://npm.jsr.io\n")
            .unwrap_or_else(|e| fail(&format!("cannot write .npmrc: {e}")));
        let target = format!("{}@{}", parsed.name, parsed.version);
        println!("[inka] pkg tar: resolving {target} (network)…");
        let mut cmd = Command::new("npm");
        cmd.current_dir(&work)
            .args(["install", "--no-save", "--omit=dev", &target]);
        if let Err(e) = run_ok(&mut cmd, "npm install") {
            let _ = fs::remove_dir_all(&work);
            fail(&format!("{e} (package managers other than npm may be used with care)"));
        }
        if !work.join("node_modules").is_dir() {
            let _ = fs::remove_dir_all(&work);
            fail("npm install did not produce a node_modules directory");
        }

        // 2) package the closure under packages/<name>/<version>/node_modules
        let pkg_dir = staging
            .join("packages")
            .join(&parsed.name)
            .join(&parsed.version);
        copy_dir(&work.join("node_modules"), &pkg_dir.join("node_modules"))
            .unwrap_or_else(|e| fail(&e));

        // 3) tar it up (gzip) + sha256 sidecar.
        let tar_file = out.join(file_base(&parsed));
        let tar_tmp = out.join(format!(".{}.tmp{}", file_base(&parsed), std::process::id()));
        let _ = fs::remove_file(&tar_tmp);
        let mut cmd = Command::new("tar");
        cmd.current_dir(&staging)
            .args(["-czf"])
            .arg(&tar_tmp)
            .arg("packages");
        if let Err(e) = run_ok(&mut cmd, "tar") {
            let _ = fs::remove_dir_all(&work);
            fail(&format!("{e}"));
        }
        fs::rename(&tar_tmp, &tar_file).unwrap_or_else(|e| {
            let _ = fs::remove_dir_all(&work);
            fail(&format!("cannot finalize {}: {e}", tar_file.display()))
        });

        let sha = sha256_of_file(&tar_file);
        fs::write(out.join(format!("{}.sha256", file_base(&parsed))), format!("{sha}\n"))
            .unwrap_or_else(|e| fail(&format!("cannot write checksum sidecar: {e}")));

        let _ = fs::remove_dir_all(&work);
        println!(
            "[inka] pkg tar: wrote {} ({} bytes, sha256 {})",
            tar_file.display(),
            fs::metadata(&tar_file).map(|m| m.len()).unwrap_or(0),
            &sha[..12]
        );
    }
}

fn sha256_of_file(path: &Path) -> String {
    let bytes = fs::read(path).unwrap_or_default();
    hex(&Sha256::digest(&bytes))
}

// ---- seed -------------------------------------------------------------------

fn seed_manifest_path(store: &Path) -> PathBuf {
    store.join(SEED_MANIFEST)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SeedEntry {
    name: String,
    version: String,
    file: String,
    sha256: String,
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct SeedManifest {
    seeded: Vec<SeedEntry>,
}

fn load_manifest(store: &Path) -> SeedManifest {
    let p = seed_manifest_path(store);
    match fs::read_to_string(&p) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => SeedManifest::default(),
    }
}

fn cmd_seed(args: &[String]) {
    let mut from: Option<String> = None;
    let mut store = store_default();
    let mut insecure = false;
    let mut specs = Vec::new();
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
            _ => specs.push(a.clone()),
        }
    }

    let Some(base) = from.or_else(|| std::env::var("INKA_PKG_SOURCE").ok()) else {
        eprintln!("error: `inka pkg seed` needs a source (use --from <dir-or-url> or INKA_PKG_SOURCE)");
        std::process::exit(2);
    };

    let want: Vec<CliSpec> = if specs.is_empty() {
        CURATED_SPECS
            .iter()
            .map(|s| parse_cli_spec(s).unwrap_or_else(|e| fail(&e)))
            .collect()
    } else {
        specs
            .iter()
            .map(|s| parse_cli_spec(s).unwrap_or_else(|e| fail(&e)))
            .collect()
    };

    fs::create_dir_all(&store).unwrap_or_else(|e| fail(&format!("cannot create {}: {e}", store.display())));
    let mut manifest = load_manifest(&store);

    for spec in &want {
        let file = file_base(spec);
        println!("[inka] pkg seed: fetching {file} from {base}");
        let (bytes, sidecar_sha) = fetch_with_sidecar(&base, &file).unwrap_or_else(|e| {
            fail(&format!("failed to fetch {file}: {e}"))
        });
        let actual = hex(&Sha256::digest(&bytes));
        let expected = match sidecar_sha {
            Some(s) => Some(s.split_whitespace().next().unwrap_or(&s).trim().to_ascii_lowercase()),
            None if insecure => None,
            None => None,
        };
        if let Some(exp) = &expected {
            if exp != &actual {
                fail(&format!("checksum mismatch for {file}: expected {exp}, actual {actual}"));
            }
            println!("[inka] pkg seed: checksum ok ({})", &actual[..12]);
        } else if expected.is_none() && !insecure {
            eprintln!("[inka] pkg seed: no .sha256 sidecar for {file}; pass --insecure to trust it");
        }

        // write to a temp file, extract into the store, then remove it
        let tmp = store.join(format!(".{}.tmp{}", file_base(spec), std::process::id()));
        fs::write(&tmp, &bytes).unwrap_or_else(|e| fail(&format!("cannot write {}: {e}", tmp.display())));
        let mut cmd = Command::new("tar");
        cmd.args(["-xzf"]).arg(&tmp).arg("-C").arg(&store);
        if let Err(e) = run_ok(&mut cmd, "tar extract") {
            let _ = fs::remove_file(&tmp);
            fail(&e);
        }
        let _ = fs::remove_file(&tmp);

        let sha = expected.clone().unwrap_or(actual.clone());
        if let Some(existing) = manifest.seeded.iter_mut().find(|e| e.name == spec.name && e.version == spec.version) {
            existing.file = file.clone();
            existing.sha256 = sha;
        } else {
            manifest.seeded.push(SeedEntry {
                name: spec.name.clone(),
                version: spec.version.clone(),
                file: file.clone(),
                sha256: sha,
            });
        }
        println!("[inka] pkg seed: installed {}@{}", spec.name, spec.version);
    }

    let json = serde_json::to_string_pretty(&manifest).unwrap_or_else(|e| fail(&format!("manifest encode: {e}")));
    let mp = seed_manifest_path(&store);
    let tmp = mp.with_extension("json.tmp");
    fs::write(&tmp, &json).unwrap_or_else(|e| fail(&format!("cannot write {}: {e}", mp.display())));
    fs::rename(&tmp, &mp).unwrap_or_else(|e| fail(&format!("cannot finalize {}: {e}", mp.display())));
}

// ---- list -------------------------------------------------------------------

fn is_version_dir(name: &str) -> bool {
    let mut parts = name.split('.');
    matches!(
        (parts.next().and_then(|p| p.parse::<u64>().ok()),
         parts.next().and_then(|p| p.parse::<u64>().ok()),
         parts.next().and_then(|p| p.parse::<u64>().ok()),
         parts.next()),
        (Some(_), Some(_), Some(_), None)
    )
}

/// Recursively list installed packages under a store's `packages` dir,
/// printing `name@version` (scoped npm names nest one level: `@jsr/std__assert`).
fn collect_packages(base: &Path, prefix: &str, out: &mut Vec<String>) {
    let mut dirs: Vec<PathBuf> = fs::read_dir(base)
        .map(|e| e.flatten().map(|d| d.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default();
    dirs.sort();
    for p in dirs {
        let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let subnames: Vec<String> = fs::read_dir(&p)
            .map(|e| {
                e.flatten()
                    .filter(|c| c.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .map(|c| c.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        if subnames.iter().any(|c| is_version_dir(c)) {
            let mut versions = subnames.clone();
            versions.sort();
            for v in versions {
                if is_version_dir(&v) {
                    out.push(format!("{prefix}{name}@{v}"));
                }
            }
        } else {
            collect_packages(&p, &format!("{prefix}{name}/"), out);
        }
    }
}

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
            _ => {
                eprintln!("error: unknown `inka pkg list` argument '{a}'");
                std::process::exit(2);
            }
        }
    }
    let mut rows = Vec::new();
    collect_packages(&store.join("packages"), "", &mut rows);
    rows.sort();
    if rows.is_empty() {
        println!("(no packages in store {})", store.display());
        return;
    }
    for r in rows {
        println!("{r}");
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
        "tar" => cmd_tar(rest),
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

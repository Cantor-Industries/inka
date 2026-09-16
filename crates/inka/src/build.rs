// inka build: bundle an entry (and its dependencies) into one self-contained
// module, then pack it onto the launcher with an embedded manifest.
//
//   inka build [source] [-s|--source <file>] [-o|--output <file>]
//             [--runtime <spec>] [--tested-against <ver>]
//             [-A|--allow-all] [-R|-W|-N|-E|-S[=list]]
//             [--allow-<cat>[=list]] [--deny-<cat>[=list]] [-P[=<set>]]
//             [--minify] [--sourcemap] [--external <pkg>]... [--embed-dir]
//
// Defaults:
//   source    first positional argument (or -s/--source)
//   output    source path with its final extension stripped (app.js -> app)
//   manifest  always derived from package.json / deno.json(.jsonc) and embedded;
//             there is no on-disk manifest input
//   launcher  $INKA_LAUNCHER, else <dir of inka binary>/inka-launcher
//   embed     the bundle; --embed-dir also embeds the whole cwd tree (assets)

use std::env;
#[cfg(feature = "bundle")]
use std::fs;
#[cfg(feature = "bundle")]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::help::{self, Mode};
use crate::permissions::{self, Flags, PermFlag};
use crate::ui;

#[cfg(feature = "bundle")]
const FOOTER_LEN: usize = 24;
#[cfg(feature = "bundle")]
const MAGIC_V4: &[u8] = b"INKFOOT5"; // bundle + optional embedded files
#[cfg(feature = "bundle")]
const LAUNCHER_BIN: &str = "inka-launcher";

/// A usage error (bad/unknown option, missing argument): print to stderr and
/// exit 2, so CI and callers do not mistake it for success.
fn usage_err(msg: &str) -> ! {
    ui::log_error(msg);
    eprintln!("usage: inka build [source] [options]");
    ui::hint("run `inka build --help` for details");
    std::process::exit(2);
}

fn err(msg: &str) -> ! {
    ui::log_error(msg);
    std::process::exit(1);
}

/// Reject an output path that would clobber the source or follow a symlink.
/// Uses canonical identity when the paths exist, so `./app.ts` and `app.ts`
/// (or a symlink to the source) are caught, not just exact string equality.
fn check_output(source: &Path, output: &Path) -> Result<(), String> {
    if let Ok(md) = std::fs::symlink_metadata(output) {
        if md.file_type().is_symlink() {
            return Err(format!(
                "output '{}' is a symlink; refusing to overwrite it",
                output.display()
            ));
        }
    }
    let same = match (std::fs::canonicalize(source), std::fs::canonicalize(output)) {
        (Ok(a), Ok(b)) => a == b,
        _ => output == source,
    };
    if same {
        return Err(format!(
            "output '{}' would overwrite the source file; pass a different -o",
            output.display()
        ));
    }
    Ok(())
}

pub fn cmd_build(args: &[String]) {
    let mut source_flag: Option<PathBuf> = None;
    let mut output_flag: Option<PathBuf> = None;
    let mut runtime_flag: Option<String> = None;
    let mut tested_flag: Option<String> = None;
    let mut perm_flags = Flags::default();
    let mut minify = false;
    let mut sourcemap = false;
    let mut external: Vec<String> = Vec::new();
    let mut embed_dir = false;
    let mut positional: Vec<PathBuf> = Vec::new();

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-s" | "--source" => source_flag = Some(next_val(&mut it, a)),
            "-o" | "--output" => output_flag = Some(next_val(&mut it, a)),
            "--runtime" => runtime_flag = Some(next_str(&mut it, a)),
            "--tested-against" => tested_flag = Some(next_str(&mut it, a)),
            "--minify" => minify = true,
            "--sourcemap" => sourcemap = true,
            "--external" => external.push(next_str(&mut it, a)),
            _ if a.starts_with("--external=") => {
                external.push(a["--external=".len()..].to_string())
            }
            "--embed-dir" => embed_dir = true,
            "-h" => {
                help::print(help::build(), Mode::Short);
                std::process::exit(0);
            }
            "--help" => {
                help::print(help::build(), Mode::Long);
                std::process::exit(0);
            }
            "-q" | "--quiet" | "-v" | "--verbose" => {
                ui::apply_verbosity_flag(a);
            }
            other if other.starts_with('-') => {
                match permissions::parse_perm_flag(&mut perm_flags, other) {
                    Ok(PermFlag::Once) => {}
                    Ok(PermFlag::ConsumeNext) => {
                        perm_flags.permset = Some(next_str(&mut it, other));
                    }
                    Ok(PermFlag::Not) => usage_err(&format!("unknown option '{other}'")),
                    Err(e) => usage_err(&e),
                }
            }
            other => positional.push(PathBuf::from(other)),
        }
    }

    if let Err(e) = permissions::validate(&perm_flags) {
        ui::log_error(&e);
        std::process::exit(2);
    }

    // Bare `-P` selects the `default` set. If it was followed by extra
    // positionals, the user likely meant `-P <name>`; point at the working form
    // before the generic "too many arguments" error.
    if perm_flags.permset.as_deref() == Some("default") && positional.len() >= 2 {
        ui::hint("bare -P selects the `default` set; use -P=<name> or --permission-set <name>");
    }

    let source = match source_flag {
        Some(s) => {
            if !positional.is_empty() {
                ui::log_error(format!(
                    "unexpected argument '{}' (source already given with -s/--source)",
                    positional[0].display()
                ));
                std::process::exit(2);
            }
            s
        }
        None => match positional.len() {
            0 => {
                ui::log_error("no source file given (pass a file or -s/--source <file>)");
                std::process::exit(2);
            }
            1 => positional.remove(0),
            _ => {
                ui::log_error(format!("too many arguments: {}", positional[1].display()));
                std::process::exit(2);
            }
        },
    };

    if !source.is_file() {
        err(&format!("source file not found: {}", source.display()));
    }

    let output = match output_flag {
        Some(o) => o,
        None => strip_extension(&source).unwrap_or_else(|| {
            err(&format!(
                "cannot derive an output name from '{}' (no extension); pass -o <file>",
                source.display()
            ))
        }),
    };

    if let Err(e) = check_output(&source, &output) {
        err(&e);
    }

    let cwd = env::current_dir()
        .unwrap_or_else(|e| err(&format!("cannot determine current directory: {e}")));
    let (manifest_bytes, manifest_warnings) = match resolve_manifest(
        &cwd,
        runtime_flag.as_deref(),
        tested_flag.as_deref(),
        &perm_flags,
    ) {
        Ok(v) => v,
        Err(e) => err(&e),
    };
    for w in &manifest_warnings {
        ui::warn(w);
    }

    let entry_rel = match crate::embed::rel_from_cwd(&cwd, &source) {
        Ok(r) => r,
        Err(e) => err(&e),
    };

    pack(
        &cwd,
        &entry_rel,
        &output,
        manifest_bytes,
        &external,
        minify,
        sourcemap,
        embed_dir,
    );
}

/// Bundle `entry_rel` and pack it (plus any embedded files) into an INKFOOT5
/// artifact at `output`.
#[allow(clippy::too_many_arguments)]
fn pack(
    cwd: &Path,
    entry_rel: &str,
    output: &Path,
    manifest_bytes: Vec<u8>,
    external: &[String],
    minify: bool,
    sourcemap: bool,
    embed_dir: bool,
) {
    #[cfg(not(feature = "bundle"))]
    {
        let _ = (
            cwd,
            entry_rel,
            output,
            manifest_bytes,
            external,
            minify,
            sourcemap,
            embed_dir,
        );
        err("inka was built without bundling support (rebuild with `--features bundle`)");
    }

    #[cfg(feature = "bundle")]
    {
        let inka_bundler::Bundle {
            code,
            embedded,
            warnings,
            auto_embed,
        } = inka_bundler::bundle(inka_bundler::BundleOptions {
            cwd,
            entry: entry_rel,
            external,
            minify,
            sourcemap,
        })
        .unwrap_or_else(|e| err(&e));

        for w in &warnings {
            ui::warn(w);
        }

        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        files.push(("main.js".to_string(), code.into_bytes()));
        for (rel, bytes) in embedded {
            push_unique(&mut files, rel, bytes);
        }
        let entry_dir = cwd.join(entry_rel);
        let entry_dir = entry_dir.parent().unwrap_or(cwd);
        // Embed user `--external` packages plus any default-external package the
        // bundle actually imports (e.g. `typescript`), found via the workspace
        // node_modules (hoisted or nested). Dedup so an explicit `--external
        // typescript` is not collected twice.
        let mut embed_pkgs: Vec<String> = external.to_vec();
        for pkg in auto_embed {
            if !embed_pkgs.contains(&pkg) {
                embed_pkgs.push(pkg);
            }
        }
        for pkg in &embed_pkgs {
            for (rel, bytes) in
                crate::embed::collect_package(cwd, entry_dir, pkg).unwrap_or_else(|e| err(&e))
            {
                push_unique(&mut files, rel, bytes);
            }
        }
        if embed_dir {
            for (rel, bytes) in
                crate::embed::collect_directory(cwd, entry_rel).unwrap_or_else(|e| err(&e))
            {
                push_unique(&mut files, rel, bytes);
            }
        }

        let module = "main.js";
        let bundle_len = files[0].1.len();
        let archive = encode_archive(&files);
        let mut manifest_payload = manifest_bytes;
        manifest_set_key(&mut manifest_payload, "module", module);
        let launcher = find_launcher();
        let launcher_bytes = fs::read(&launcher)
            .unwrap_or_else(|e| err(&format!("cannot read launcher {}: {e}", launcher.display())));

        let mut out = Vec::with_capacity(
            launcher_bytes.len() + archive.len() + manifest_payload.len() + FOOTER_LEN,
        );
        out.extend_from_slice(&launcher_bytes);
        out.extend_from_slice(&archive);
        out.extend_from_slice(&manifest_payload);
        out.extend_from_slice(MAGIC_V4);
        out.extend_from_slice(&(archive.len() as u64).to_le_bytes());
        out.extend_from_slice(&(manifest_payload.len() as u64).to_le_bytes());

        write_executable(output, &out)
            .unwrap_or_else(|e| err(&format!("cannot write {}: {e}", output.display())));

        ui::title("build");
        ui::section("Build");
        ui::row("entry", entry_rel);
        ui::row("module", module);
        ui::row("output", output.display());
        ui::row("launcher", launcher.display());
        ui::section("Bundle");
        ui::row("code", ui::human_size(bundle_len as u64));
        ui::row("embedded", format!("{} file(s)", files.len() - 1));
        ui::row("manifest", ui::human_size(manifest_payload.len() as u64));
        ui::row("artifact", ui::human_size(out.len() as u64));
        ui::status_ok(format!(
            "packed {} ({})",
            output.display(),
            ui::human_size(out.len() as u64)
        ));
    }
}

#[cfg(feature = "bundle")]
fn push_unique(files: &mut Vec<(String, Vec<u8>)>, rel: String, bytes: Vec<u8>) {
    if !files.iter().any(|(r, _)| r == &rel) {
        files.push((rel, bytes));
    }
}

/// Write an executable atomically: a `create_new` temp file in the output
/// directory (a pre-planted symlink fails rather than being followed), chmod
/// 0755, then rename into place. A failed build never leaves a partial output.
#[cfg(feature = "bundle")]
fn write_executable(output: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = match output.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = output
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "out".to_string());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(".{name}.tmp{}-{nanos}", std::process::id()));

    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    if let Err(e) = f.write_all(bytes) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    drop(f);
    if let Err(e) = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp, output) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Encode files as `{path_len u64}{data_len u64}{path}{data}` entries.
#[cfg(feature = "bundle")]
fn encode_archive(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (path, data) in files {
        out.extend_from_slice(&(path.len() as u64).to_le_bytes());
        out.extend_from_slice(&(data.len() as u64).to_le_bytes());
        out.extend_from_slice(path.as_bytes());
        out.extend_from_slice(data);
    }
    out
}

fn next_val(it: &mut std::slice::Iter<'_, String>, flag: &str) -> PathBuf {
    match it.next() {
        Some(v) => PathBuf::from(v),
        None => usage_err(&format!("{flag} requires a value")),
    }
}

fn next_str(it: &mut std::slice::Iter<'_, String>, flag: &str) -> String {
    match it.next() {
        Some(v) => v.clone(),
        None => usage_err(&format!("{flag} requires a value")),
    }
}

fn strip_extension(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let (stem, _) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    let mut out = path.to_path_buf();
    out.set_file_name(stem);
    Some(out)
}

fn manifest_has_key(bytes: &[u8], key: &str) -> bool {
    String::from_utf8_lossy(bytes)
        .lines()
        .any(|l| l.trim_start().starts_with(&format!("{key}=")))
}

/// Replace (or append) a `key=` line in a manifest buffer.
fn manifest_set_key(bytes: &mut Vec<u8>, key: &str, value: &str) {
    let text = String::from_utf8_lossy(bytes);
    let mut replaced = false;
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with(&format!("{key}=")) {
            out.push(format!("{key}={value}"));
            replaced = true;
        } else {
            out.push(line.to_string());
        }
    }
    if !replaced {
        if !out.is_empty() {
            out.push(String::new()); // blank line separator
        }
        out.push(format!("{key}={value}"));
    }
    let mut joined = out.join("\n");
    if !joined.ends_with('\n') {
        joined.push('\n');
    }
    *bytes = joined.into_bytes();
}

/// Derive the embedded manifest from project config and CLI overrides. There is
/// no on-disk manifest: permission lines come from explicit CLI flags (which
/// override config) or package.json / deno.json(.jsonc) build-intent sources,
/// and the runtime requirement from config plus `--runtime`/`--tested-against`.
/// Returns the manifest bytes and any non-fatal warnings.
/// Validate a `--runtime`/`--tested-against` value: no newline (manifest
/// injection) and the version grammar the launcher understands.
fn check_version_arg(flag: &str, value: &str) -> Result<(), String> {
    if !crate::config::valid_version_spec(value) {
        return Err(format!(
            "{flag} expects a version like 0.266.2 (optionally >=/==), got '{value}'"
        ));
    }
    Ok(())
}

fn resolve_manifest(
    cwd: &Path,
    runtime_flag: Option<&str>,
    tested_flag: Option<&str>,
    perm_flags: &Flags,
) -> Result<(Vec<u8>, Vec<String>), String> {
    // Must track `crates/inka-runtime/runtime-version`: an artifact must never
    // select a runtime too old to enforce its permission DSL or resolution.
    const DEFAULT_RUNTIME: &str = ">=0.266.7";

    // CLI permission flags override any config-derived permission source.
    let (cli_dsl, cli_warns) = if perm_flags.selects() {
        permissions::dsl(cwd, perm_flags)?
    } else {
        (String::new(), Vec::new())
    };
    let cli_dsl = perm_flags.selects().then_some(cli_dsl.as_str());

    let syn = crate::config::synthesize_manifest(cwd, None, cli_dsl)?;
    let mut bytes = syn.bytes;
    let mut warnings = cli_warns;
    warnings.extend(syn.warnings);

    // Runtime precedence: --runtime > config `inka.runtime` > default floor. The
    // floor is always embedded so an artifact can never select a runtime too old
    // to enforce its permission DSL.
    if let Some(r) = runtime_flag {
        check_version_arg("--runtime", r)?;
        manifest_set_key(&mut bytes, "runtime", &crate::config::runtime_value(r));
    } else if !manifest_has_key(&bytes, "runtime") {
        manifest_set_key(
            &mut bytes,
            "runtime",
            &crate::config::runtime_value(DEFAULT_RUNTIME),
        );
    }
    if let Some(t) = tested_flag {
        check_version_arg("--tested-against", t)?;
        manifest_set_key(&mut bytes, "tested-against", t);
    }
    Ok((bytes, warnings))
}

#[cfg(feature = "bundle")]
fn find_launcher() -> PathBuf {
    if let Ok(p) = env::var("INKA_LAUNCHER") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return p;
        }
        err(&format!(
            "INKA_LAUNCHER points to a missing file: {}",
            p.display()
        ));
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            let adjacent = dir.join(LAUNCHER_BIN);
            if adjacent.is_file() {
                return adjacent;
            }
        }
    }
    err(&format!(
        "cannot find the '{LAUNCHER_BIN}' launcher (build it with `cargo build --release -p inka-launcher`, \
         keep it next to this inka binary, or set INKA_LAUNCHER)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn scratch() -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("inkabuild-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(cwd: &Path, name: &str, body: &str) {
        fs::write(cwd.join(name), body).unwrap();
    }

    fn flags(pset: Option<&str>) -> Flags {
        Flags {
            permset: pset.map(str::to_string),
            ..Default::default()
        }
    }

    fn manifest(
        cwd: &Path,
        runtime: Option<&str>,
        tested: Option<&str>,
        pset: Option<&str>,
    ) -> String {
        let (bytes, _) = resolve_manifest(cwd, runtime, tested, &flags(pset)).unwrap();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn default_runtime_floor_always_embedded() {
        let cwd = scratch();
        let m = manifest(&cwd, None, None, None);
        assert!(m.contains("runtime=inka_runtime>=0.266.7"), "{m}");
        assert!(!m.contains("allow-"), "{m}");
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn runtime_flag_overrides_default_and_config() {
        let cwd = scratch();
        write(
            &cwd,
            "deno.json",
            r#"{ "inka": { "runtime": ">=0.300.0" } }"#,
        );
        // config `inka.runtime` wins over the default floor...
        let m = manifest(&cwd, None, None, None);
        assert!(m.contains("runtime=inka_runtime>=0.300.0"), "{m}");
        // ...but --runtime beats config.
        let m = manifest(&cwd, Some("==0.266.0"), None, None);
        assert!(m.contains("runtime=inka_runtime==0.266.0"), "{m}");
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn tested_against_flag_embedded() {
        let cwd = scratch();
        let m = manifest(&cwd, None, Some("0.266.0"), None);
        assert!(m.contains("tested-against=0.266.0"), "{m}");
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn permission_set_is_baked() {
        let cwd = scratch();
        write(
            &cwd,
            "deno.json",
            r#"{ "permissions": { "server": { "net": ["0.0.0.0:80"] } } }"#,
        );
        let m = manifest(&cwd, None, None, Some("server"));
        assert!(m.contains("allow-net=0.0.0.0:80"), "{m}");
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn cli_allow_all_bakes_permissions_all() {
        let cwd = scratch();
        let f = Flags {
            allow_all: true,
            ..Default::default()
        };
        let (bytes, _) = resolve_manifest(&cwd, None, None, &f).unwrap();
        let m = String::from_utf8(bytes).unwrap();
        assert!(m.contains("permissions=all"), "{m}");
        assert!(m.contains("runtime=inka_runtime>=0.266.7"), "{m}");
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn cli_grant_overrides_config_permissions() {
        let cwd = scratch();
        write(
            &cwd,
            "deno.json",
            r#"{ "compile": { "permissions": { "read": ["./data"] } } }"#,
        );
        let f = Flags {
            allow: vec![("env".to_string(), "*".to_string())],
            ..Default::default()
        };
        let (bytes, _) = resolve_manifest(&cwd, None, None, &f).unwrap();
        let m = String::from_utf8(bytes).unwrap();
        assert!(m.contains("allow-env=*"), "{m}");
        assert!(!m.contains("allow-read"), "CLI should override config: {m}");
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn check_output_rejects_source_and_symlink() {
        let cwd = scratch();
        write(&cwd, "app.ts", "console.log(1);\n");
        let src = cwd.join("app.ts");
        // Exact and `./`-prefixed self-overwrite.
        assert!(check_output(&src, &src).is_err());
        assert!(check_output(&src, &cwd.join("./app.ts")).is_err());
        // A distinct output is fine.
        assert!(check_output(&src, &cwd.join("app")).is_ok());
        // A symlink pointing at the source must be refused.
        let link = cwd.join("link");
        std::os::unix::fs::symlink(&src, &link).unwrap();
        assert!(check_output(&src, &link).is_err());
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn module_line_appended_and_replaced() {
        let mut appended = b"runtime=x\n".to_vec();
        manifest_set_key(&mut appended, "module", "app.js");
        assert_eq!(
            String::from_utf8(appended).unwrap(),
            "runtime=x\n\nmodule=app.js\n"
        );
        let mut replaced = b"module=old.js\n".to_vec();
        manifest_set_key(&mut replaced, "module", "sub/main.ts");
        assert_eq!(String::from_utf8(replaced).unwrap(), "module=sub/main.ts\n");
    }
}

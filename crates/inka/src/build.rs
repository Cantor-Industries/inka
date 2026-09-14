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

use crate::permissions::{self, Flags, PermFlag};

#[cfg(feature = "bundle")]
const FOOTER_LEN: usize = 24;
#[cfg(feature = "bundle")]
const MAGIC_V4: &[u8] = b"INKFOOT5"; // bundle + optional embedded files
#[cfg(feature = "bundle")]
const LAUNCHER_BIN: &str = "inka-launcher";

fn help() -> ! {
    println!(
        "usage: inka build [source] [-s|--source <file>] [-o|--output <file>] [--runtime <spec>] [--tested-against <ver>] [-A|--allow-all] [-R|-W|-N|-E|-S[=list]] [--allow-<cat>[=list]] [--deny-<cat>[=list]] [-P[=<set>]] [--minify] [--sourcemap] [--external <pkg>]... [--embed-dir]\n\
         \n\
         bundles <source> (import maps + npm:/jsr:/node_modules) into one self-contained\n\
         module and packs it onto the launcher. The manifest is always derived from\n\
         package.json / deno.json(.jsonc) permissions and embedded; there is no on-disk\n\
         manifest input.\n\
         \n\
         options:\n\
         \x20 -s, --source <file>   source file (default: the positional argument)\n\
         \x20 -o, --output <file>   output executable (default: source without its extension)\n\
         \x20     --runtime <spec>  runtime requirement, e.g. '>=0.266.2' or '==0.266.2' (overrides config)\n\
         \x20     --tested-against <ver>  never roll forward past this runtime (overrides config)\n\
         \x20 -A, --allow-all       bake permissions=all (trimmed by any --deny-*)\n\
         \x20 -R, -W, -N, -E, -S    bake read/write/net/env/sys (whole category); -R=<list> scopes it\n\
         \x20     --allow-<cat>[=list]   bake a grant for read|write|net|env|run|sys|ffi\n\
         \x20     --deny-<cat>[=list]    deny within an allowed category\n\
         \x20 -P[=<name>], --permission-set[=<name>]  bake a named config set (bare -P = `default`)\n\
         \x20     --minify          minify the bundle\n\
         \x20     --sourcemap       embed an inline source map\n\
         \x20     --external <pkg>  leave a package unbundled and embed it from node_modules (repeatable)\n\
         \x20     --embed-dir       also embed the whole current-directory tree (for assets)\n\
         \x20 -h, --help            show this help\n\
         \n\
         CLI permission flags override config-derived permissions. Without any source\n\
         the artifact is deny-by-default.\n\
         \n\
         launcher is found at $INKA_LAUNCHER or next to the inka binary."
    );
    std::process::exit(0);
}

fn err(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1);
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
            "-h" | "--help" => help(),
            other if other.starts_with('-') => {
                match permissions::parse_perm_flag(&mut perm_flags, other) {
                    Ok(PermFlag::Once) => {}
                    Ok(PermFlag::ConsumeNext) => {
                        perm_flags.permset = Some(next_str(&mut it, other));
                    }
                    Ok(PermFlag::Not) => {
                        eprintln!("error: unknown option '{other}'");
                        help();
                    }
                    Err(e) => {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    }
                }
            }
            other => positional.push(PathBuf::from(other)),
        }
    }

    if let Err(e) = permissions::validate(&perm_flags) {
        eprintln!("error: {e}");
        std::process::exit(2);
    }

    // Bare `-P` selects the `default` set. If it was followed by extra
    // positionals, the user likely meant `-P <name>`; point at the working form
    // before the generic "too many arguments" error.
    if perm_flags.permset.as_deref() == Some("default") && positional.len() >= 2 {
        eprintln!(
            "note: bare -P selects the `default` set; use -P=<name> or \
             --permission-set <name> to pick another set"
        );
    }

    let source = match source_flag {
        Some(s) => {
            if !positional.is_empty() {
                eprintln!(
                    "error: unexpected argument '{}' (source already given with -s/--source)",
                    positional[0].display()
                );
                std::process::exit(2);
            }
            s
        }
        None => match positional.len() {
            0 => {
                eprintln!("error: no source file given (pass a file or -s/--source <file>)");
                std::process::exit(2);
            }
            1 => positional.remove(0),
            _ => {
                eprintln!("error: too many arguments: {}", positional[1].display());
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

    if output == source {
        err(&format!(
            "output '{}' would overwrite the source file; pass a different -o",
            output.display()
        ));
    }

    let cwd = env::current_dir()
        .unwrap_or_else(|e| err(&format!("cannot determine current directory: {e}")));
    let (manifest_bytes, manifest_warnings) = resolve_manifest(
        &cwd,
        runtime_flag.as_deref(),
        tested_flag.as_deref(),
        &perm_flags,
    );
    for w in &manifest_warnings {
        eprintln!("warning: {w}");
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
        let bundle = inka_bundler::bundle(inka_bundler::BundleOptions {
            cwd,
            entry: entry_rel,
            external,
            minify,
            sourcemap,
        })
        .unwrap_or_else(|e| err(&e));

        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        files.push(("main.js".to_string(), bundle.code.into_bytes()));
        for (rel, bytes) in bundle.embedded {
            push_unique(&mut files, rel, bytes);
        }
        for pkg in external {
            for (rel, bytes) in crate::embed::collect_package(cwd, pkg).unwrap_or_else(|e| err(&e))
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
        let manifest_payload = set_module_line(&manifest_bytes, module);
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

        fs::write(output, &out)
            .unwrap_or_else(|e| err(&format!("cannot write {}: {e}", output.display())));
        fs::set_permissions(output, fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|e| err(&format!("cannot chmod {}: {e}", output.display())));

        println!(
            "packed {} ({}) <- launcher {} ({}) + bundle ({}) + {} embedded file(s) ({}) + manifest ({})",
            output.display(),
            out.len(),
            launcher.display(),
            launcher_bytes.len(),
            bundle_len,
            files.len() - 1,
            archive.len(),
            manifest_payload.len(),
        );
        println!("  entry: {entry_rel}  (module {module})");
    }
}

#[cfg(feature = "bundle")]
fn push_unique(files: &mut Vec<(String, Vec<u8>)>, rel: String, bytes: Vec<u8>) {
    if !files.iter().any(|(r, _)| r == &rel) {
        files.push((rel, bytes));
    }
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

/// Force the `module=` line to the packed entry path. The manifest is always
/// derived from config (which never emits `module=`), so this replaces a stale
/// line if present and appends otherwise.
#[cfg(feature = "bundle")]
fn set_module_line(manifest: &[u8], entry: &str) -> Vec<u8> {
    let text = String::from_utf8_lossy(manifest);
    let mut found = false;
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("module=") {
            found = true;
            let lead_len = line.len() - line.trim_start().len();
            out.push(format!("{}module={entry}", &line[..lead_len]));
        } else {
            out.push(line.to_string());
        }
    }
    if !found {
        out.push(format!("module={entry}"));
    }
    let mut joined = out.join("\n");
    if text.ends_with('\n') {
        joined.push('\n');
    }
    joined.into_bytes()
}

fn next_val(it: &mut std::slice::Iter<'_, String>, flag: &str) -> PathBuf {
    match it.next() {
        Some(v) => PathBuf::from(v),
        None => {
            eprintln!("error: {flag} requires a value");
            std::process::exit(2);
        }
    }
}

fn next_str(it: &mut std::slice::Iter<'_, String>, flag: &str) -> String {
    match it.next() {
        Some(v) => v.clone(),
        None => {
            eprintln!("error: {flag} requires a value");
            std::process::exit(2);
        }
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

/// Turn a runtime spec like `>=0.266.2` / `==0.266.2` / `0.266.2` into a
/// `runtime=inka_runtime…` value.
fn runtime_value(spec: &str) -> String {
    let spec = spec.trim();
    if spec.starts_with('>') || spec.starts_with('=') {
        format!("inka_runtime{spec}")
    } else {
        format!("inka_runtime=={spec}")
    }
}

/// Derive the embedded manifest from project config and CLI overrides. There is
/// no on-disk manifest: permission lines come from explicit CLI flags (which
/// override config) or package.json / deno.json(.jsonc) build-intent sources,
/// and the runtime requirement from config plus `--runtime`/`--tested-against`.
/// Returns the manifest bytes and any non-fatal warnings.
fn resolve_manifest(
    cwd: &Path,
    runtime_flag: Option<&str>,
    tested_flag: Option<&str>,
    perm_flags: &Flags,
) -> (Vec<u8>, Vec<String>) {
    // Must track `crates/inka-runtime/runtime-version`: an older runtime needs
    // the retired resolver, which this toolchain no longer installs.
    const DEFAULT_RUNTIME: &str = ">=0.266.2";

    // CLI permission flags override any config-derived permission source.
    let (cli_dsl, cli_warns) = if perm_flags.selects() {
        permissions::dsl(cwd, perm_flags)
    } else {
        (String::new(), Vec::new())
    };
    let cli_dsl = perm_flags.selects().then_some(cli_dsl.as_str());

    let syn = crate::config::synthesize_manifest(cwd, None, cli_dsl);
    let mut bytes = syn.bytes;
    let mut warnings = cli_warns;
    warnings.extend(syn.warnings);

    // Runtime precedence: --runtime > config `inka.runtime` > default floor. The
    // floor is always embedded so an artifact can never select a runtime too old
    // to enforce its permission DSL.
    if let Some(r) = runtime_flag {
        manifest_set_key(&mut bytes, "runtime", &runtime_value(r));
    } else if !manifest_has_key(&bytes, "runtime") {
        manifest_set_key(&mut bytes, "runtime", &runtime_value(DEFAULT_RUNTIME));
    }
    if let Some(t) = tested_flag {
        manifest_set_key(&mut bytes, "tested-against", t);
    }
    (bytes, warnings)
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
        let (bytes, _) = resolve_manifest(cwd, runtime, tested, &flags(pset));
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn default_runtime_floor_always_embedded() {
        let cwd = scratch();
        let m = manifest(&cwd, None, None, None);
        assert!(m.contains("runtime=inka_runtime>=0.266.2"), "{m}");
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
        let (bytes, _) = resolve_manifest(&cwd, None, None, &f);
        let m = String::from_utf8(bytes).unwrap();
        assert!(m.contains("permissions=all"), "{m}");
        assert!(m.contains("runtime=inka_runtime>=0.266.2"), "{m}");
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
        let (bytes, _) = resolve_manifest(&cwd, None, None, &f);
        let m = String::from_utf8(bytes).unwrap();
        assert!(m.contains("allow-env=*"), "{m}");
        assert!(!m.contains("allow-read"), "CLI should override config: {m}");
        let _ = fs::remove_dir_all(&cwd);
    }

    #[cfg(feature = "bundle")]
    #[test]
    fn module_line_appended_and_replaced() {
        let appended = set_module_line(b"runtime=x\n", "app.js");
        assert_eq!(
            String::from_utf8(appended).unwrap(),
            "runtime=x\nmodule=app.js\n"
        );
        let replaced = set_module_line(b"module=old.js\n", "sub/main.ts");
        assert_eq!(String::from_utf8(replaced).unwrap(), "module=sub/main.ts\n");
    }
}

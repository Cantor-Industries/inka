// inka build: pack one or more source files + an embedded manifest onto the launcher.
//
//   inka build [source] [-s|--source <file>] [-o|--output <file>]
//             [--runtime <spec>] [--tested-against <ver>] [-P <set>]
//             [--transpile] [--embed-dir]
//
// Defaults:
//   source    first positional argument (or -s/--source)
//   output    source path with its final extension stripped (app.js -> app)
//   manifest  always derived from package.json / deno.json(.jsonc) and embedded;
//             there is no on-disk manifest input
//   launcher  $INKA_LAUNCHER, else <dir of inka binary>/inka-launcher
//   embed     import closure by default; --embed-dir embeds the whole cwd tree

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const FOOTER_LEN: usize = 24;
const MAGIC_V1: &[u8] = b"INKFOOT2"; // single embedded source
const MAGIC_V2: &[u8] = b"INKFOOT3"; // multi-file archive
const MAGIC_V3: &[u8] = b"INKFOOT4"; // multi-file archive, TS already transpiled
const LAUNCHER_BIN: &str = "inka-launcher";

fn help() -> ! {
    println!(
        "usage: inka build [source] [-s|--source <file>] [-o|--output <file>] [--runtime <spec>] [--tested-against <ver>] [-P <name>] [--transpile] [--embed-dir]\n\
         \n\
         packs <source> (and the files it imports) onto the launcher into a single executable.\n\
         The manifest is always derived from package.json / deno.json(.jsonc) permissions\n\
         and embedded; there is no on-disk manifest input.\n\
         \n\
         options:\n\
         \x20 -s, --source <file>   source file (default: the positional argument)\n\
         \x20 -o, --output <file>   output executable (default: source without its extension)\n\
         \x20     --runtime <spec>  runtime requirement, e.g. '>=0.266.5' or '==0.266.5' (overrides config)\n\
         \x20     --tested-against <ver>  never roll forward past this runtime (overrides config)\n\
         \x20 -P, --permission-set <name>  use this named permission set from the config\n\
         \x20     --transpile       compile TypeScript to JavaScript now (single- and multi-file; default: the runtime transpiles at load)\n\
         \x20     --embed-dir       embed the whole current-directory tree (for dynamic imports) instead of just the import closure\n\
         \x20 -h, --help            show this help\n\
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
    let mut perm_set: Option<String> = None;
    let mut transpile = false;
    let mut embed_dir = false;
    let mut positional: Vec<PathBuf> = Vec::new();

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-s" | "--source" => source_flag = Some(next_val(&mut it, a)),
            "-o" | "--output" => output_flag = Some(next_val(&mut it, a)),
            "--runtime" => runtime_flag = Some(next_str(&mut it, a)),
            "--tested-against" => tested_flag = Some(next_str(&mut it, a)),
            // `-P <name>` is the documented build form (README: `-P server app.ts`),
            // so it consumes the next token; `-P=<name>` and the long forms also work.
            "-P" | "--permission-set" => perm_set = Some(next_str(&mut it, a)),
            _ if a.starts_with("-P=") => perm_set = Some(a["-P=".len()..].to_string()),
            _ if a.starts_with("--permission-set=") => {
                perm_set = Some(a["--permission-set=".len()..].to_string())
            }
            "--transpile" => transpile = true,
            "--embed-dir" => embed_dir = true,
            "-h" | "--help" => help(),
            other if other.starts_with('-') => {
                eprintln!("error: unknown option '{other}'");
                help();
            }
            other => positional.push(PathBuf::from(other)),
        }
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
        perm_set.as_deref(),
    );
    for w in &manifest_warnings {
        eprintln!("warning: {w}");
    }
    let manifest_label = "config (package.json/deno.json)";

    let entry_rel = match crate::embed::rel_from_cwd(&cwd, &source) {
        Ok(r) => r,
        Err(e) => err(&e),
    };

    let mode = if embed_dir {
        crate::embed::Mode::Directory
    } else {
        crate::embed::Mode::Closure
    };
    let mut files = match mode {
        crate::embed::Mode::Directory => crate::embed::collect_directory(&cwd, &entry_rel),
        crate::embed::Mode::Closure => crate::embed::collect(&cwd, &entry_rel),
    }
    .unwrap_or_else(|e| err(&e));

    // Auto-embed the project's node_modules closure (bring-your-own-node_modules)
    // so an artifact built from a normal Node project carries its dependencies.
    let nm_files =
        crate::embed::collect_node_modules_closure(&cwd, &entry_rel).unwrap_or_else(|e| err(&e));
    for (rel, bytes) in nm_files {
        if !files.iter().any(|(r, _)| r == &rel) {
            files.push((rel, bytes));
        }
    }

    let is_multi = files.len() > 1;

    let launcher = find_launcher();
    let launcher_bytes = fs::read(&launcher)
        .unwrap_or_else(|e| err(&format!("cannot read launcher {}: {e}", launcher.display())));

    let mut out = Vec::new();

    if is_multi {
        if transpile && jsx_family(&entry_rel) {
            err(&format!(
                "--transpile does not support '{entry_rel}' yet (only .ts/.mts/.cts)"
            ));
        }

        // Build-time transpile: replace each .ts/.mts/.cts module's bytes with
        // its transpiled JS while keeping the original archive path (Deno-style).
        // The archive then carries INKFOOT4 so the runtime serves those modules
        // as plain JavaScript without re-transpiling.
        let (files, n_transpiled, precompiled) = if transpile {
            let mut n = 0usize;
            let mut out: Vec<(String, Vec<u8>)> = Vec::with_capacity(files.len());
            for (rel, bytes) in &files {
                if ts_family(rel) {
                    let text = String::from_utf8_lossy(bytes).into_owned();
                    let js = crate::transpile::ts_to_js(&text, rel).unwrap_or_else(|e| err(&e));
                    n += 1;
                    out.push((rel.clone(), js.into_bytes()));
                } else {
                    out.push((rel.clone(), bytes.clone()));
                }
            }
            (out, n, n > 0)
        } else {
            (files, 0, false)
        };
        let magic = if precompiled { MAGIC_V3 } else { MAGIC_V2 };

        let archive = encode_archive(&files);
        let manifest_payload = set_module_line(&manifest_bytes, &entry_rel);
        out.reserve(launcher_bytes.len() + archive.len() + manifest_payload.len() + FOOTER_LEN);
        out.extend_from_slice(&launcher_bytes);
        out.extend_from_slice(&archive);
        out.extend_from_slice(&manifest_payload);
        out.extend_from_slice(magic);
        out.extend_from_slice(&(archive.len() as u64).to_le_bytes());
        out.extend_from_slice(&(manifest_payload.len() as u64).to_le_bytes());
        fs::write(&output, &out)
            .unwrap_or_else(|e| err(&format!("cannot write {}: {e}", output.display())));
        fs::set_permissions(&output, fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|e| err(&format!("cannot chmod {}: {e}", output.display())));
        println!(
            "packed {} ({}) <- launcher {} ({}) + {} file(s) archive ({}) + manifest {} ({})",
            output.display(),
            out.len(),
            launcher.display(),
            launcher_bytes.len(),
            files.len(),
            archive.len(),
            manifest_label,
            manifest_payload.len(),
        );
        if precompiled {
            println!(
                "  entry: {entry_rel}  ({} files embedded, {n_transpiled} transpiled to JS)",
                files.len()
            );
        } else {
            println!("  entry: {entry_rel}  ({} files embedded)", files.len());
        }
        return;
    }

    // ---- single-file build (back-compatible v1 trailer) --------------------
    let entry_bytes = files.first().map(|(_, b)| b.clone()).unwrap_or_default();
    let source_name = Path::new(&entry_rel)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "main.js".into());
    let ts_source = ts_family(&source_name);

    let mut payload = entry_bytes;
    let mut transpiled = false;
    if transpile {
        if ts_source {
            let text = String::from_utf8_lossy(&payload).into_owned();
            let js = match crate::transpile::ts_to_js(&text, &source_name) {
                Ok(s) => s,
                Err(e) => err(&e),
            };
            payload = js.into_bytes();
            transpiled = true;
        } else if jsx_family(&source_name) {
            err(&format!(
                "--transpile does not support '{source_name}' yet (only .ts/.mts/.cts)"
            ));
        }
    }

    let module_name = if transpiled {
        Path::new(&source_name)
            .with_extension("js")
            .to_string_lossy()
            .into_owned()
    } else {
        source_name.clone()
    };
    let manifest_payload = set_module_line(&manifest_bytes, &module_name);

    out.reserve(launcher_bytes.len() + payload.len() + manifest_payload.len() + FOOTER_LEN);
    out.extend_from_slice(&launcher_bytes);
    out.extend_from_slice(&payload);
    out.extend_from_slice(&manifest_payload);
    out.extend_from_slice(MAGIC_V1);
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&(manifest_payload.len() as u64).to_le_bytes());

    fs::write(&output, &out)
        .unwrap_or_else(|e| err(&format!("cannot write {}: {e}", output.display())));
    fs::set_permissions(&output, fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|e| err(&format!("cannot chmod {}: {e}", output.display())));

    println!(
        "packed {} ({}) <- launcher {} ({}) + source {} ({}{}) + manifest {} ({})",
        output.display(),
        out.len(),
        launcher.display(),
        launcher_bytes.len(),
        source.display(),
        payload.len(),
        if transpiled { ", transpiled to JS" } else { "" },
        manifest_label,
        manifest_payload.len(),
    );
}

/// Encode files as `{path_len u64}{data_len u64}{path}{data}` entries.
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

fn ext_of(name: &str) -> Option<String> {
    name.rsplit('.').next().map(|e| e.to_ascii_lowercase())
}

fn ts_family(name: &str) -> bool {
    matches!(ext_of(name).as_deref(), Some("ts" | "mts" | "cts"))
}

fn jsx_family(name: &str) -> bool {
    matches!(ext_of(name).as_deref(), Some("tsx" | "jsx"))
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

/// Turn a runtime spec like `>=0.266.5` / `==0.266.5` / `0.266.5` into a
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
/// no on-disk manifest: permission lines and the runtime requirement come only
/// from package.json / deno.json(.jsonc), plus `--runtime`/`--tested-against`.
/// Returns the manifest bytes and any non-fatal warnings.
fn resolve_manifest(
    cwd: &Path,
    runtime_flag: Option<&str>,
    tested_flag: Option<&str>,
    perm_set: Option<&str>,
) -> (Vec<u8>, Vec<String>) {
    // Must track `crates/inka-runtime/runtime-version`: an older runtime needs
    // the retired resolver, which this toolchain no longer installs.
    const DEFAULT_RUNTIME: &str = ">=0.266.5";

    let syn = crate::config::synthesize_manifest(cwd, perm_set);
    let mut bytes = syn.bytes;

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
    (bytes, syn.warnings)
}

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

    fn manifest(
        cwd: &Path,
        runtime: Option<&str>,
        tested: Option<&str>,
        pset: Option<&str>,
    ) -> String {
        let (bytes, _) = resolve_manifest(cwd, runtime, tested, pset);
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn default_runtime_floor_always_embedded() {
        let cwd = scratch();
        let m = manifest(&cwd, None, None, None);
        assert!(m.contains("runtime=inka_runtime>=0.266.5"), "{m}");
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

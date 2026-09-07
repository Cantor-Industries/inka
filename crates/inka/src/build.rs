// inka build: pack one or more source files + a manifest onto the launcher.
//
//   inka build [source] [-s|--source <file>] [-o|--output <file>] [--manifest <file>]
//             [--transpile] [--embed-dir]
//
// Defaults:
//   source    first positional argument (or -s/--source)
//   output    source path with its final extension stripped (app.js -> app)
//   manifest  --manifest, else <source-stem>.manifest then inka.manifest in cwd
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
        "usage: inka build [source] [-s|--source <file>] [-o|--output <file>] [--manifest <file>] [--transpile] [--embed-dir]\n\
         \n\
         packs <source> (and the files it imports) onto the launcher into a single executable.\n\
         \n\
         options:\n\
         \x20 -s, --source <file>   source file (default: the positional argument)\n\
         \x20 -o, --output <file>   output executable (default: source without its extension)\n\
         \x20     --manifest <file> manifest file (default: <source-stem>.manifest, then inka.manifest,\n\
         \x20                      then auto-generated from package.json / deno.json permissions)\n\
         \x20     --runtime <spec>  runtime requirement, e.g. '>=0.266.0' or '==0.266.0' (overrides config)\n\
         \x20     --tested-against <ver>  never roll forward past this runtime (overrides config)\n\
         \x20 -P, --permission-set <name>  use this named permission set from the config\n\
         \x20     --transpile       compile TypeScript to JavaScript now (single- and multi-file; default: the runtime transpiles at load)\n\
         \x20     --embed-dir       embed the whole current-directory tree (for dynamic imports) instead of just the import closure\n\
         \x20     --vendor-closure  embed only the vendored modules reachable from the entry's import graph\n\
         \x20     --no-vendor       skip vendored embedding entirely (artifact relies on the machine default store)\n\
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
    let mut manifest_flag: Option<PathBuf> = None;
    let mut runtime_flag: Option<String> = None;
    let mut tested_flag: Option<String> = None;
    let mut perm_set: Option<String> = None;
    let mut transpile = false;
    let mut embed_dir = false;
    let mut vendor_closure = false;
    let mut no_vendor = false;
    let mut positional: Vec<PathBuf> = Vec::new();

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-s" | "--source" => source_flag = Some(next_val(&mut it, a)),
            "-o" | "--output" => output_flag = Some(next_val(&mut it, a)),
            "--manifest" => manifest_flag = Some(next_val(&mut it, a)),
            "--runtime" => runtime_flag = Some(next_str(&mut it, a)),
            "--tested-against" => tested_flag = Some(next_str(&mut it, a)),
            "-P" | "--permission-set" => perm_set = Some(next_str(&mut it, a)),
            "--transpile" => transpile = true,
            "--embed-dir" => embed_dir = true,
            "--vendor-closure" => vendor_closure = true,
            "--no-vendor" => no_vendor = true,
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
                eprintln!("error: unexpected argument '{}' (source already given with -s/--source)", positional[0].display());
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
    let (manifest_bytes, manifest_label) = resolve_manifest(
        &source,
        manifest_flag.as_deref(),
        &cwd,
        runtime_flag.as_deref(),
        tested_flag.as_deref(),
        perm_set.as_deref(),
    );

    let entry_rel = match crate::embed::rel_from_cwd(&cwd, &source) {
        Ok(r) => r,
        Err(e) => err(&e),
    };

    if vendor_closure && no_vendor {
        err("--vendor-closure and --no-vendor are mutually exclusive");
    }
    if (vendor_closure || no_vendor) && embed_dir {
        err("--vendor-closure/--no-vendor do not apply with --embed-dir (whole-tree embed already includes vendored/)");
    }

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

    // Embed the per-project vendored package roots so a built artifact carries
    // its overrides/extras. Default is the whole pool; `--vendor-closure` embeds
    // only the vendored modules reachable from the entry graph; `--no-vendor`
    // skips vendored embedding (the artifact uses the machine default store).
    // The launcher auto-detects a `vendored/` dir under the extracted root.
    let vendor_files = if no_vendor {
        Vec::new()
    } else if vendor_closure {
        crate::embed::collect_vendored_closure(&cwd, &entry_rel).unwrap_or_else(|e| err(&e))
    } else {
        crate::embed::collect_vendored(&cwd).unwrap_or_else(|e| err(&e))
    };
    let has_vendor = !vendor_files.is_empty();
    for (rel, bytes) in vendor_files {
        if !files.iter().any(|(r, _)| r == &rel) {
            files.push((rel, bytes));
        }
    }

    let is_multi = files.len() > 1 || has_vendor;

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
    let entry_bytes = files
        .first()
        .map(|(_, b)| b.clone())
        .unwrap_or_default();
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

    let (manifest_payload, module_warning) =
        effective_manifest(&source_name, &manifest_bytes, transpile, ts_source);
    if let Some(w) = module_warning {
        eprintln!("warning: {w}");
    }

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

/// Force the `module=` line to a given entry path (used for multi-file builds,
/// where the entry lives at a cwd-relative path, not a bare filename).
fn set_module_line(manifest: &[u8], entry_rel: &str) -> Vec<u8> {
    let text = String::from_utf8_lossy(manifest);
    let mut found = false;
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(val) = trimmed.strip_prefix("module=") {
            found = true;
            let val = val.trim();
            if val != entry_rel {
                eprintln!(
                    "warning: manifest 'module={val}' overridden to entry '{entry_rel}' (required for multi-file artifacts)"
                );
            }
            let lead_len = line.len() - line.trim_start().len();
            out.push(format!("{}module={entry_rel}", &line[..lead_len]));
        } else {
            out.push(line.to_string());
        }
    }
    if !found {
        out.push(format!("module={entry_rel}"));
    }
    let mut joined = out.join("\n");
    if text.ends_with('\n') {
        joined.push('\n');
    }
    joined.into_bytes()
}

fn ext_of(name: &str) -> Option<String> {
    name.rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
}

fn ts_family(name: &str) -> bool {
    matches!(ext_of(name).as_deref(), Some("ts" | "mts" | "cts"))
}

fn jsx_family(name: &str) -> bool {
    matches!(ext_of(name).as_deref(), Some("tsx" | "jsx"))
}

/// Returns the effective manifest: the original bytes, but with a canonical
/// `module=` line matching what is actually packed.
///
/// - No `module=` line -> append one derived from the source name
///   (`.ts`/`.mts`/`.cts` preserved for runtime transpile; `.js` when
///   `--transpile` already produced JavaScript).
/// - Explicit `module=` conflicting with the packed code's language -> warn.
fn effective_manifest(
    source_name: &str,
    manifest: &[u8],
    transpiled_to_js: bool,
    ts_source: bool,
) -> (Vec<u8>, Option<String>) {
    let text = String::from_utf8_lossy(manifest);
    let mut found_module = false;
    let mut warning = None;

    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(val) = trimmed.strip_prefix("module=") {
            found_module = true;
            let val = val.trim().to_string();
            let mut newval = val.clone();
            if transpiled_to_js {
                if ts_family(&val) {
                    newval = Path::new(&val).with_extension("js").to_string_lossy().into();
                }
            } else if ts_source && !ts_family(&val) {
                warning = Some(format!(
                    "manifest 'module={val}' is not a TypeScript name but source is '{source_name}'; \
                     the runtime will not transpile it. Set module={source_name}, omit the module= line, \
                     or pass --transpile."
                ));
            }
            let lead_len = line.len() - line.trim_start().len();
            out.push(format!("{}module={newval}", &line[..lead_len]));
        } else {
            out.push(line.to_string());
        }
    }
    if !found_module {
        let module = if transpiled_to_js {
            Path::new(source_name)
                .with_extension("js")
                .to_string_lossy()
                .into_owned()
        } else {
            source_name.to_string()
        };
        out.push(format!("module={module}"));
    }

    let mut joined = out.join("\n");
    if text.ends_with('\n') {
        joined.push('\n');
    }
    (joined.into_bytes(), warning)
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

fn find_default_manifest(source: &Path) -> Option<PathBuf> {
    let cwd = env::current_dir().ok()?;
    let stem = source.file_stem()?.to_string_lossy();
    let stem_manifest = cwd.join(format!("{stem}.manifest"));
    if stem_manifest.is_file() {
        return Some(stem_manifest);
    }
    let generic = cwd.join("inka.manifest");
    if generic.is_file() {
        return Some(generic);
    }
    None
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

/// Turn a runtime spec like `>=0.266.0` / `==0.266.0` / `0.266.0` into a
/// `runtime=inka_runtime…` value.
fn runtime_value(spec: &str) -> String {
    let spec = spec.trim();
    if spec.starts_with('>') || spec.starts_with('=') {
        format!("inka_runtime{spec}")
    } else {
        format!("inka_runtime=={spec}")
    }
}

/// Resolve the effective manifest bytes + a display label.
/// 1) `--manifest <file>`  2) on-disk `*.manifest`/`inka.manifest`
/// 3) auto-synthesized from package.json / deno.json permissions
/// CLI `--runtime` / `--tested-against` override whatever was chosen.
fn resolve_manifest(
    source: &Path,
    explicit: Option<&Path>,
    cwd: &Path,
    runtime_flag: Option<&str>,
    tested_flag: Option<&str>,
    perm_set: Option<&str>,
) -> (Vec<u8>, String) {
    const DEFAULT_RUNTIME: &str = ">=0.266.0";

    // 1 & 2: an explicit/on-disk manifest wins over config.
    let chosen: Option<(Vec<u8>, String, bool)> = if let Some(p) = explicit {
        if !p.is_file() {
            err(&format!("manifest file not found: {}", p.display()));
        }
        let bytes = fs::read(p).unwrap_or_else(|e| err(&format!("cannot read {}: {e}", p.display())));
        Some((bytes, p.display().to_string(), true))
    } else if let Some(p) = find_default_manifest(source) {
        let bytes = fs::read(&p).unwrap_or_else(|e| err(&format!("cannot read {}: {e}", p.display())));
        Some((bytes, p.display().to_string(), true))
    } else {
        None
    };

    let (mut bytes, label, from_file) = match chosen {
        Some((b, l, f)) => (b, l, f),
        None => {
            let syn = crate::config::synthesize_manifest(cwd, perm_set);
            for w in &syn.warnings {
                eprintln!("warning: {w}");
            }
            (syn.bytes, "config (package.json/deno.json)".to_string(), false)
        }
    };

    // Default floor only applies to auto-synthesized manifests (a hand-written
    // .manifest may intentionally omit `runtime=` to accept any installed engine).
    if !from_file && !manifest_has_key(&bytes, "runtime") && runtime_flag.is_none() {
        manifest_set_key(&mut bytes, "runtime", &runtime_value(DEFAULT_RUNTIME));
    }
    if let Some(r) = runtime_flag {
        manifest_set_key(&mut bytes, "runtime", &runtime_value(r));
    }
    if let Some(t) = tested_flag {
        manifest_set_key(&mut bytes, "tested-against", t);
    }
    (bytes, label)
}

fn find_launcher() -> PathBuf {
    if let Ok(p) = env::var("INKA_LAUNCHER") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return p;
        }
        err(&format!("INKA_LAUNCHER points to a missing file: {}", p.display()));
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

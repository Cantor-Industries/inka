// dex build: pack a source file + manifest onto the launcher into a single
// executable artifact.
//
//   dex build [source] [-s|--source <file>] [-o|--output <file>] [--manifest <file>]
//
// Defaults:
//   source    first positional argument (or -s/--source)
//   output    source path with its final extension stripped (app.js -> app)
//   manifest  --manifest, else <source-stem>.manifest then dex.manifest in cwd
//   launcher  $DEX_LAUNCHER, else <dir of dex binary>/dex-launcher

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const FOOTER_LEN: usize = 24;
const MAGIC: &[u8] = b"DEXFOOT2";
const LAUNCHER_BIN: &str = "dex-launcher";

fn help() -> ! {
    println!(
        "usage: dex build [source] [-s|--source <file>] [-o|--output <file>] [--manifest <file>] [--transpile]\n\
         \n\
         packs <source> onto the launcher into a single executable.\n\
         \n\
         options:\n\
         \x20 -s, --source <file>   source file (default: the positional argument)\n\
         \x20 -o, --output <file>   output executable (default: source without its extension)\n\
         \x20     --manifest <file> manifest file (default: <source-stem>.manifest, then dex.manifest, in the current directory)\n\
         \x20     --transpile       compile TypeScript to JavaScript now (default: the runtime transpiles at load)\n\
         \x20 -h, --help            show this help\n\
         \n\
         launcher is found at $DEX_LAUNCHER or next to the dex binary."
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
    let mut transpile = false;
    let mut positional: Vec<PathBuf> = Vec::new();

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-s" | "--source" => source_flag = Some(next_val(&mut it, a)),
            "-o" | "--output" => output_flag = Some(next_val(&mut it, a)),
            "--manifest" => manifest_flag = Some(next_val(&mut it, a)),
            "--transpile" => transpile = true,
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

    let manifest = match manifest_flag {
        Some(m) => m,
        None => find_default_manifest(&source).unwrap_or_else(|| {
            let stem = source
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "?".into());
            err(&format!(
                "no manifest found (looked for '{stem}.manifest' and 'dex.manifest' in {})",
                env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "the current directory".into())
            ))
        }),
    };
    if !manifest.is_file() {
        err(&format!("manifest file not found: {}", manifest.display()));
    }

    let source_bytes = fs::read(&source)
        .unwrap_or_else(|e| err(&format!("cannot read {}: {e}", source.display())));
    let manifest_bytes = fs::read(&manifest)
        .unwrap_or_else(|e| err(&format!("cannot read {}: {e}", manifest.display())));

    let source_name = source
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "main.js".into());
    let ts_source = ts_family(&source_name);

    let mut payload = source_bytes;
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

    let launcher = find_launcher();
    let launcher_bytes = fs::read(&launcher)
        .unwrap_or_else(|e| err(&format!("cannot read launcher {}: {e}", launcher.display())));

    let plen = payload.len() as u64;
    let mlen = manifest_payload.len() as u64;

    let mut out = Vec::with_capacity(
        launcher_bytes.len() + payload.len() + manifest_payload.len() + FOOTER_LEN,
    );
    out.extend_from_slice(&launcher_bytes);
    out.extend_from_slice(&payload);
    out.extend_from_slice(&manifest_payload);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&plen.to_le_bytes());
    out.extend_from_slice(&mlen.to_le_bytes());

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
        manifest.display(),
        manifest_payload.len(),
    );
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

fn next_val(it: &mut std::slice::Iter<'_, String>, flag: &str) -> PathBuf {    match it.next() {
        Some(v) => PathBuf::from(v),
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
    let generic = cwd.join("dex.manifest");
    if generic.is_file() {
        return Some(generic);
    }
    None
}

fn find_launcher() -> PathBuf {
    if let Ok(p) = env::var("DEX_LAUNCHER") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return p;
        }
        err(&format!("DEX_LAUNCHER points to a missing file: {}", p.display()));
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
        "cannot find the '{LAUNCHER_BIN}' launcher (build it with `cargo build --release -p launcher`, \
         keep it next to this dex binary, or set DEX_LAUNCHER)"
    ))
}

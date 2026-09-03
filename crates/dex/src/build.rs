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
        "usage: dex build [source] [-s|--source <file>] [-o|--output <file>] [--manifest <file>]\n\
         \n\
         packs <source> onto the launcher into a single executable.\n\
         \n\
         options:\n\
         \x20 -s, --source <file>   source file (default: the positional argument)\n\
         \x20 -o, --output <file>   output executable (default: source without its extension)\n\
         \x20     --manifest <file> manifest file (default: <source-stem>.manifest, then dex.manifest, in the current directory)\n\
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
    let mut positional: Vec<PathBuf> = Vec::new();

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-s" | "--source" => source_flag = Some(next_val(&mut it, a)),
            "-o" | "--output" => output_flag = Some(next_val(&mut it, a)),
            "--manifest" => manifest_flag = Some(next_val(&mut it, a)),
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

    let launcher = find_launcher();
    let launcher_bytes = fs::read(&launcher)
        .unwrap_or_else(|e| err(&format!("cannot read launcher {}: {e}", launcher.display())));

    let plen = source_bytes.len() as u64;
    let mlen = manifest_bytes.len() as u64;

    let mut out = Vec::with_capacity(
        launcher_bytes.len() + source_bytes.len() + manifest_bytes.len() + FOOTER_LEN,
    );
    out.extend_from_slice(&launcher_bytes);
    out.extend_from_slice(&source_bytes);
    out.extend_from_slice(&manifest_bytes);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&plen.to_le_bytes());
    out.extend_from_slice(&mlen.to_le_bytes());

    fs::write(&output, &out)
        .unwrap_or_else(|e| err(&format!("cannot write {}: {e}", output.display())));
    fs::set_permissions(&output, fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|e| err(&format!("cannot chmod {}: {e}", output.display())));

    println!(
        "packed {} ({}) <- launcher {} ({}) + source {} ({}) + manifest {} ({})",
        output.display(),
        out.len(),
        launcher.display(),
        launcher_bytes.len(),
        source.display(),
        source_bytes.len(),
        manifest.display(),
        manifest_bytes.len(),
    );
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

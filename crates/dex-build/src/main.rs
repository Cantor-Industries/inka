use std::env;
use std::fs;
use std::path::PathBuf;

const FOOTER_LEN: usize = 24;
const MAGIC: &[u8] = b"DEXFOOT2";

fn usage() -> ! {
    eprintln!("usage: dex-build --launcher <exe> --source <file> --manifest <file> --output <file>");
    std::process::exit(2);
}

fn main() {
    let mut launcher = None;
    let mut source = None;
    let mut manifest = None;
    let mut output = None;

    let mut it = env::args().skip(1);
    while let Some(arg) = it.next() {
        let val = it.next().unwrap_or_else(|| usage());
        match arg.as_str() {
            "--launcher" => launcher = Some(PathBuf::from(val)),
            "--source" => source = Some(PathBuf::from(val)),
            "--manifest" => manifest = Some(PathBuf::from(val)),
            "--output" => output = Some(PathBuf::from(val)),
            _ => usage(),
        }
    }

    let (launcher, source, manifest, output) = match (launcher, source, manifest, output) {
        (Some(a), Some(b), Some(c), Some(d)) => (a, b, c, d),
        _ => usage(),
    };

    let launcher_bytes = fs::read(&launcher).unwrap_or_else(|e| {
        eprintln!("error reading launcher {}: {e}", launcher.display());
        std::process::exit(1);
    });
    let source_bytes = fs::read(&source).unwrap_or_else(|e| {
        eprintln!("error reading source {}: {e}", source.display());
        std::process::exit(1);
    });
    let manifest_bytes = fs::read(&manifest).unwrap_or_else(|e| {
        eprintln!("error reading manifest {}: {e}", manifest.display());
        std::process::exit(1);
    });

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

    fs::write(&output, &out).unwrap_or_else(|e| {
        eprintln!("error writing {}: {e}", output.display());
        std::process::exit(1);
    });

    println!(
        "packed {} ({}) <- launcher {} + source {} ({}) + manifest {} ({})",
        output.display(),
        out.len(),
        launcher_bytes.len(),
        source.display(),
        source_bytes.len(),
        manifest.display(),
        manifest_bytes.len(),
    );
}

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

mod bundle_esm;
mod postprocess;
mod spec;

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut spec_path: Option<String> = None;
    let mut node_modules: Option<String> = None;
    let mut it = args.iter();
    if let Some(first) = it.next() {
        if first == "apply" {
            // accepted subcommand token (inka pkg snapshot invokes `apply --spec …`)
        } else if first == "--help" || first == "-h" {
            eprintln!(
                "usage: inka-patcher apply --spec <patch.json> --node-modules <store node_modules>\n\
                 applies a repo-managed patch (bundle-esm | file-patch) in place to an installed package"
            );
            return Ok(());
        } else {
            it = args.iter(); // tolerate calling without the `apply` token
        }
    }
    while let Some(a) = it.next() {
        match a.as_str() {
            "--spec" => spec_path = Some(it.next().context("--spec needs a file")?.clone()),
            "--node-modules" => {
                node_modules = Some(it.next().context("--node-modules needs a dir")?.clone())
            }
            "--help" | "-h" => {
                eprintln!(
                    "usage: inka-patcher apply --spec <patch.json> --node-modules <store node_modules>\n\
                     applies a repo-managed patch (bundle-esm | file-patch) in place to an installed package"
                );
                return Ok(());
            }
            other => bail!("unknown argument '{other}'"),
        }
    }
    let spec_path = spec_path.context("missing --spec")?;
    let node_modules = node_modules.context("missing --node-modules")?;

    let spec = spec::PatchSpec::load(&PathBuf::from(&spec_path))?;
    let pkg_dir = spec.package_dir(&PathBuf::from(&node_modules))?;
    match spec.kind.as_str() {
        "bundle-esm" => apply_bundle_esm(&spec, &pkg_dir),
        "file-patch" => apply_file_patch(&spec, &pkg_dir),
        other => bail!("unknown patch type '{other}' (expected bundle-esm | file-patch)"),
    }
}

fn apply_bundle_esm(spec: &spec::PatchSpec, pkg_dir: &std::path::Path) -> Result<()> {
    let entry = spec.entry.as_deref().context("bundle-esm patch needs an entry")?;
    let outfile = spec.output.as_deref().context("bundle-esm patch needs an output")?;
    if !pkg_dir.join(entry).is_file() {
        bail!("entry {entry} missing in {}", pkg_dir.display());
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    let code = rt.block_on(bundle_esm::bundle_to_esm(pkg_dir, entry, &spec.external))?;
    let code = postprocess::postprocess(&code, &spec.neutralize_env)
        .context("postprocess bundle")?;
    std::fs::write(pkg_dir.join(outfile), code)
        .with_context(|| format!("write {}/{}", pkg_dir.display(), outfile))?;
    postprocess::point_exports_at_esm(pkg_dir, outfile)?;
    println!(
        "[inka-patcher] {}@{} -> {outfile} (bundle-esm, {} B)",
        spec.package,
        spec.version,
        std::fs::metadata(pkg_dir.join(outfile)).map(|m| m.len()).unwrap_or(0)
    );
    Ok(())
}

fn apply_file_patch(spec: &spec::PatchSpec, pkg_dir: &std::path::Path) -> Result<()> {
    let file = spec.file.as_deref().context("file-patch patch needs a file")?;
    let path = pkg_dir.join(file);
    let raw = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let text = String::from_utf8(raw).context("patch target is not utf-8")?;
    match spec.delete_from_marker.as_deref() {
        Some(marker) => {
            let idx = text.find(marker).with_context(|| {
                format!("deleteFromMarker not found in {}: {marker:?}", path.display())
            })?;
            let patched = &text[..idx];
            std::fs::write(&path, patched).with_context(|| format!("write {}", path.display()))?;
            println!(
                "[inka-patcher] {}@{} -> {} truncated at marker ({} -> {} B)",
                spec.package,
                spec.version,
                file,
                text.len(),
                patched.len()
            );
        }
        None => bail!("file-patch needs deleteFromMarker"),
    }
    Ok(())
}

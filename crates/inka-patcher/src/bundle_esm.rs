use std::path::Path;

use anyhow::{bail, Context, Result};
use rolldown::{BundlerBuilder, BundlerOptions, InputItem, IsExternal, OutputFormat, Platform};

/// Bundle a package entry to a single self-contained ESM module (in memory).
pub async fn bundle_to_esm(pkg_dir: &Path, entry: &str, external: &[String]) -> Result<String> {
    if !pkg_dir.is_dir() {
        bail!("bundle input dir missing: {}", pkg_dir.display());
    }
    let options = BundlerOptions {
        input: Some(vec![InputItem {
            name: Some("chunk".to_string()),
            import: entry.to_string(),
        }]),
        cwd: Some(pkg_dir.to_path_buf()),
        platform: Some(Platform::Node),
        external: Some(IsExternal::from(external.to_vec())),
        format: Some(OutputFormat::Esm),
        ..Default::default()
    };
    let mut bundler = BundlerBuilder::default()
        .with_options(options)
        .build()
        .context("construct rolldown bundler")?;
    let bundle_output = bundler
        .generate()
        .await
        .context("rolldown generate")?;

    let mut chunks = Vec::new();
    for asset in &bundle_output.assets {
        if let rolldown_common::Output::Chunk(chunk) = asset {
            chunks.push(chunk.code.clone());
        }
    }
    if chunks.len() != 1 {
        bail!(
            "expected a single output chunk for {entry}, rolldown produced {}",
            chunks.len()
        );
    }
    Ok(chunks.into_iter().next().unwrap())
}

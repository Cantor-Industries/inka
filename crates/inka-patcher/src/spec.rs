use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// One repo-managed patch spec under `patches/<pkg>/<version>/patch.json`.
#[derive(Debug, Deserialize)]
pub struct PatchSpec {
    pub package: String,
    pub version: String,
    #[serde(rename = "type")]
    pub kind: String, // "bundle-esm" | "file-patch"
    /// bundle-esm: entry module inside the package root (e.g. "wrapper.mjs").
    #[serde(default)]
    pub entry: Option<String>,
    /// bundle-esm: specifiers to leave external (optional natives etc.).
    #[serde(default)]
    pub external: Vec<String>,
    /// bundle-esm: output file written into the package root.
    #[serde(default)]
    pub output: Option<String>,
    /// bundle-esm: `process.env.<NAME>` reads to neutralize -> `"1"` (per-package).
    #[serde(default, rename = "neutralizeEnv")]
    pub neutralize_env: Vec<String>,
    /// file-patch: file inside the package root to rewrite.
    #[serde(default)]
    pub file: Option<String>,
    /// file-patch: truncate from the first occurrence of this marker (incl.) to EOF.
    #[serde(default, rename = "deleteFromMarker")]
    pub delete_from_marker: Option<String>,
}

impl PatchSpec {
    pub fn load(path: &Path) -> Result<PatchSpec> {
        let raw = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let spec: PatchSpec = serde_json::from_slice(&raw)
            .with_context(|| format!("parse patch spec {}", path.display()))?;
        Ok(spec)
    }

    /// Validate the spec and that it targets the installed package version.
    pub fn package_dir(&self, node_modules: &Path) -> Result<std::path::PathBuf> {
        if self.package.is_empty() {
            bail!("patch spec: empty package name");
        }
        if self.package.starts_with('/') || self.package.ends_with('/') {
            bail!("patch spec: bad package name '{}'", self.package);
        }
        let dir = node_modules.join(&self.package);
        if !dir.is_dir() {
            bail!("patched package {} is not installed at {}", self.package, dir.display());
        }
        let raw = std::fs::read(dir.join("package.json"))
            .with_context(|| format!("read {}/package.json", dir.display()))?;
        let v: serde_json::Value = serde_json::from_slice(&raw)
            .with_context(|| format!("parse {}/package.json", dir.display()))?;
        let actual = v
            .get("version")
            .and_then(serde_json::Value::as_str)
            .with_context(|| format!("{}/package.json has no version", dir.display()))?;
        if actual != self.version {
            bail!(
                "patch {}@{} found installed {}@{} (re-seed/align versions)",
                self.package,
                self.version,
                self.package,
                actual
            );
        }
        Ok(dir)
    }
}

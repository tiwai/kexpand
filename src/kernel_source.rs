use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Reads series.conf, patch files, and rpm/config.sh directly from a
/// kernel-source working tree (the current directory or a given root path).
pub struct KernelSource {
    root: PathBuf,
}

impl KernelSource {
    pub fn open(root: &Path) -> Result<Self> {
        if !root.join("series.conf").exists() {
            bail!("series.conf not found in {}", root.display());
        }
        Ok(KernelSource { root: root.to_path_buf() })
    }

    /// Read SRCVERSION from rpm/config.sh and return it as a git tag (e.g. "v6.4").
    pub fn base_tag(&self) -> Result<String> {
        let bytes = fs::read(self.root.join("rpm/config.sh"))
            .context("reading rpm/config.sh")?;
        parse_base_tag(&bytes)
    }

    /// Parse series.conf and return ordered patch paths.
    pub fn series(&self) -> Result<Vec<PathBuf>> {
        let bytes = fs::read(self.root.join("series.conf"))
            .context("reading series.conf")?;
        crate::series::parse_series(&bytes)
    }

    /// Read all patch files in parallel directly from the working tree.
    pub fn read_patches(&self, series: &[PathBuf]) -> Result<Vec<(PathBuf, Vec<u8>)>> {
        use rayon::prelude::*;
        let root = &self.root;
        series.par_iter()
            .map(|p| {
                let content = fs::read(root.join(p))
                    .with_context(|| format!("reading patch {}", p.display()))?;
                Ok((p.clone(), content))
            })
            .collect()
    }
}

/// Extract SRCVERSION from the contents of rpm/config.sh as a git tag (e.g. "v6.4").
pub fn parse_base_tag(bytes: &[u8]) -> Result<String> {
    let content = std::str::from_utf8(bytes).context("rpm/config.sh is not UTF-8")?;
    for line in content.lines() {
        if let Some(val) = line.strip_prefix("SRCVERSION=") {
            return Ok(format!("v{}", val.trim()));
        }
    }
    bail!("SRCVERSION not found in rpm/config.sh")
}

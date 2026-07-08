use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use git2::{Oid, Repository};

/// Reads series.conf, patch files, and rpm/config.sh from a specific branch
/// of kernel-source.git without requiring a working-tree checkout.
pub struct KernelSource {
    repo: Repository,
    tree_oid: Oid,
}

impl KernelSource {
    pub fn open(path: &Path, branch: &str) -> Result<Self> {
        let repo = Repository::open(path)
            .with_context(|| format!("opening kernel-source at {}", path.display()))?;
        let refname = format!("refs/heads/{branch}");
        let tree_oid = {
            let reference = repo.find_reference(&refname)
                .with_context(|| format!("branch '{branch}' not found in {}", path.display()))?;
            reference.peel_to_commit()?.tree_id()
        };
        Ok(KernelSource { repo, tree_oid })
    }

    fn read_blob(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        let tree = self.repo.find_tree(self.tree_oid).context("finding tree")?;
        match tree.get_path(path) {
            Ok(entry) => {
                let obj = entry.to_object(&self.repo)
                    .with_context(|| format!("reading object '{}'", path.display()))?;
                let blob = obj.as_blob()
                    .ok_or_else(|| anyhow!("'{}' is not a blob", path.display()))?;
                Ok(Some(blob.content().to_vec()))
            }
            Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("looking up '{}'", path.display())),
        }
    }

    /// Read SRCVERSION from rpm/config.sh and return it as a git tag (e.g. "v6.4").
    pub fn base_tag(&self) -> Result<String> {
        let bytes = self.read_blob(Path::new("rpm/config.sh"))?
            .context("rpm/config.sh not found in branch")?;
        let content = std::str::from_utf8(&bytes).context("rpm/config.sh is not UTF-8")?;
        for line in content.lines() {
            if let Some(val) = line.strip_prefix("SRCVERSION=") {
                return Ok(format!("v{}", val.trim()));
            }
        }
        bail!("SRCVERSION not found in rpm/config.sh")
    }

    /// Parse series.conf from the branch and return ordered patch paths.
    pub fn series(&self) -> Result<Vec<PathBuf>> {
        let bytes = self.read_blob(Path::new("series.conf"))?
            .context("series.conf not found in branch")?;
        crate::series::parse_series(&bytes)
    }

    /// Read all patch blobs for the given series entries.
    /// Returns (relative_path, raw_bytes) in the same order as `series`.
    pub fn read_patches(&self, series: &[PathBuf]) -> Result<Vec<(PathBuf, Vec<u8>)>> {
        series.iter().map(|p| {
            let bytes = self.read_blob(p)?
                .with_context(|| format!("patch not found in branch: {}", p.display()))?;
            Ok((p.clone(), bytes))
        }).collect()
    }
}

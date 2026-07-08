use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
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
                    .ok_or_else(|| anyhow::anyhow!("'{}' is not a blob", path.display()))?;
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

    /// Read all patch blobs for the given series entries using parallel sharded
    /// `git cat-file --batch` processes, one per CPU (capped at 8). Each shard
    /// inflates a contiguous slice of the series in its own git process, so
    /// zlib inflate scales across cores. Results are flattened in series order.
    pub fn read_patches(&self, series: &[PathBuf]) -> Result<Vec<(PathBuf, Vec<u8>)>> {
        if series.is_empty() {
            return Ok(Vec::new());
        }

        let num_shards = rayon::current_num_threads().min(8).max(1);
        let chunk_size = series.len().div_ceil(num_shards);

        let repo_git_dir = self.repo.path().to_path_buf();
        let tree_oid_str = self.tree_oid.to_string();

        let chunks: Vec<&[PathBuf]> = series.chunks(chunk_size).collect();

        use rayon::prelude::*;
        let shard_results: Vec<Result<Vec<(PathBuf, Vec<u8>)>>> = chunks
            .into_par_iter()
            .map(|shard| {
                Self::read_patches_shard(&repo_git_dir, &tree_oid_str, shard)
            })
            .collect();

        let mut results = Vec::with_capacity(series.len());
        for chunk in shard_results {
            results.extend(chunk?);
        }
        Ok(results)
    }

    /// Read one contiguous slice of the series via a single `git cat-file --batch`.
    fn read_patches_shard(
        repo_git_dir: &Path,
        tree_oid_str: &str,
        shard: &[PathBuf],
    ) -> Result<Vec<(PathBuf, Vec<u8>)>> {
        use std::io::{BufRead, BufReader, BufWriter, Read, Write};
        use std::process::{Command, Stdio};

        let mut child = Command::new("git")
            .arg("--git-dir").arg(repo_git_dir)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("spawning git cat-file")?;

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        // Write "<tree>:<path>" specs to stdin in a background thread to avoid
        // pipe deadlock when the spec data exceeds the OS pipe buffer.
        let specs: Vec<String> = shard.iter()
            .map(|p| format!("{tree_oid_str}:{}", p.display()))
            .collect();

        let write_handle = std::thread::spawn(move || -> std::io::Result<()> {
            let mut w = BufWriter::new(stdin);
            for spec in &specs {
                w.write_all(spec.as_bytes())?;
                w.write_all(b"\n")?;
            }
            Ok(())
        });

        let mut reader = BufReader::with_capacity(1 << 20, stdout);
        let mut results = Vec::with_capacity(shard.len());
        let mut header_buf = String::new();

        for path in shard {
            header_buf.clear();
            reader.read_line(&mut header_buf).context("reading cat-file header")?;
            let header = header_buf.trim_end();

            if header.ends_with(" missing") {
                bail!("patch not found in branch: {}", path.display());
            }

            let size: usize = header
                .rsplit_once(' ')
                .and_then(|(_, s)| s.parse().ok())
                .with_context(|| format!("malformed cat-file header: {header:?}"))?;

            let mut content = vec![0u8; size];
            reader.read_exact(&mut content).context("reading blob content")?;
            reader.read_exact(&mut [0u8]).context("reading object separator")?;

            results.push((path.clone(), content));
        }

        write_handle.join().unwrap().context("writing cat-file stdin")?;
        let status = child.wait().context("waiting for git cat-file")?;
        if !status.success() {
            bail!("git cat-file --batch failed");
        }

        Ok(results)
    }
}

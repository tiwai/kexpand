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

    /// Read all patch blobs for the given series entries using `git cat-file --batch`.
    /// Sequential packfile access via git's native bulk reader is much faster than
    /// individual libgit2 object lookups for large numbers of objects.
    /// Returns (relative_path, raw_bytes) in the same order as `series`.
    pub fn read_patches(&self, series: &[PathBuf]) -> Result<Vec<(PathBuf, Vec<u8>)>> {
        use std::io::{BufRead, BufReader, BufWriter, Write};
        use std::process::{Command, Stdio};

        if series.is_empty() {
            return Ok(Vec::new());
        }

        let repo_git_dir = self.repo.path().to_path_buf();
        let tree_oid_str = self.tree_oid.to_string();

        use std::io::Read;

        let mut child = Command::new("git")
            .arg("--git-dir").arg(&repo_git_dir)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("spawning git cat-file")?;

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        // Write "<tree>:<path>" specs to stdin in a background thread.
        // Running concurrently with the read loop below avoids pipe deadlock
        // when the combined spec data exceeds the kernel pipe buffer.
        let specs: Vec<String> = series.iter()
            .map(|p| format!("{tree_oid_str}:{}", p.display()))
            .collect();

        let write_handle = std::thread::spawn(move || -> std::io::Result<()> {
            let mut w = BufWriter::new(stdin);
            for spec in &specs {
                w.write_all(spec.as_bytes())?;
                w.write_all(b"\n")?;
            }
            Ok(()) // drop flushes BufWriter, then closes stdin → EOF for git cat-file
        });

        // Read blob output: each object is "<oid> <type> <size>\n<content>\n"
        let mut reader = BufReader::with_capacity(1 << 20, stdout);
        let mut results = Vec::with_capacity(series.len());
        let mut header_buf = String::new();

        for path in series {
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

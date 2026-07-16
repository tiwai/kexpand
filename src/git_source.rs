use std::cell::RefCell;
use std::path::{Path, PathBuf};
use anyhow::{anyhow, Context, Result};
use git2::{Oid, Repository};

thread_local! {
    static GS_REPO: RefCell<Option<Repository>> = const { RefCell::new(None) };
}

pub struct GitSource {
    repo: Repository,
    tree_oid: Oid,
    commit_sha1: String,
}

impl GitSource {
    /// Return the underlying `git2::Repository` for the linux source repo.
    pub fn repo(&self) -> &Repository {
        &self.repo
    }

    pub fn new(linux: &Path, base_tag: &str) -> Result<Self> {
        let repo = Repository::open(linux)
            .with_context(|| format!("opening git repo: {}", linux.display()))?;

        let (commit_sha1, tree_oid) = {
            let obj = repo.revparse_single(base_tag)
                .with_context(|| format!("resolving tag '{}'", base_tag))?;
            let commit = obj.peel_to_commit()
                .with_context(|| format!("peeling '{}' to commit", base_tag))?;
            (format!("{}", commit.id()), commit.tree_id())
        };

        Ok(GitSource { repo, tree_oid, commit_sha1 })
    }

    /// Returns the SHA1 of the base commit (e.g. for v6.4), used in git fast-import `from`.
    pub fn base_commit_sha1(&self) -> &str {
        &self.commit_sha1
    }

    /// Load content and file mode for a batch of paths in parallel.
    /// Returns (path, content_or_none, mode) for each input path.
    /// Paths not found in the tree return (path, None, 0o100644).
    pub fn prewarm_files(&self, paths: &[PathBuf]) -> Result<Vec<(PathBuf, Option<Vec<u8>>, u32)>> {
        use rayon::prelude::*;
        let repo_path = self.repo.path().to_path_buf();
        let tree_oid = self.tree_oid;

        paths.par_iter().map(|path| -> Result<(PathBuf, Option<Vec<u8>>, u32)> {
            GS_REPO.with(|cell| {
                let mut guard = cell.borrow_mut();
                if guard.is_none() {
                    *guard = Some(Repository::open(&repo_path)
                        .context("opening linux repo")?);
                }
                let repo = guard.as_ref().unwrap();
                let tree = repo.find_tree(tree_oid).context("finding base tree")?;
                match tree.get_path(path) {
                    Ok(entry) => {
                        let mode = entry.filemode() as u32;
                        let obj = entry.to_object(repo)
                            .with_context(|| format!("getting object for '{}'", path.display()))?;
                        let blob = obj.as_blob()
                            .ok_or_else(|| anyhow!("'{}' is not a blob", path.display()))?;
                        Ok((path.clone(), Some(blob.content().to_vec()), mode))
                    }
                    Err(e) if e.code() == git2::ErrorCode::NotFound =>
                        Ok((path.clone(), None, 0o100644)),
                    Err(e) =>
                        Err(e).with_context(|| format!("looking up '{}'", path.display())),
                }
            })
        }).collect()
    }
}

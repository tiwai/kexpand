use std::path::Path;
use anyhow::{anyhow, Context, Result};
use git2::{Oid, Repository};

pub struct GitSource {
    repo: Repository,
    tree_oid: Oid,
    commit_sha1: String,
}

impl GitSource {
    pub fn new(linus: &Path, base_tag: &str) -> Result<Self> {
        let repo = Repository::open(linus)
            .with_context(|| format!("opening git repo: {}", linus.display()))?;

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

    /// Lazily load a file's content from the git tree. Returns the raw bytes.
    /// Returns `None` if the path doesn't exist in the tree (new file created by patches).
    pub fn load_file(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        let tree = self.repo.find_tree(self.tree_oid)
            .context("finding base tree")?;

        match tree.get_path(path) {
            Ok(entry) => {
                let obj = entry.to_object(&self.repo)
                    .with_context(|| format!("getting object for '{}'", path.display()))?;
                let blob = obj.as_blob()
                    .ok_or_else(|| anyhow!("'{}' is not a blob", path.display()))?;
                Ok(Some(blob.content().to_vec()))
            }
            Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("looking up '{}'", path.display())),
        }
    }

    /// Get the file mode (permissions) from the git tree entry.
    /// Returns 0o100644 if not found or not a regular file.
    pub fn file_mode(&self, path: &Path) -> u32 {
        let Ok(tree) = self.repo.find_tree(self.tree_oid) else { return 0o100644 };
        let Ok(entry) = tree.get_path(path) else { return 0o100644 };
        entry.filemode() as u32
    }
}

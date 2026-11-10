use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::cmp::Ordering;

use anyhow::{bail, Context, Result};

use crate::apply::PatchResult;
use crate::commit_pack::CommitPackWriter;
use crate::patch_header::PatchMeta;

// ─── DirEntry / DirState ─────────────────────────────────────────────────────

#[derive(Clone)]
struct DirEntry {
    oid: git2::Oid,
    /// Raw git filemode: 0o100644, 0o100755, 0o120000, or 0o040000 (tree).
    filemode: u32,
}

impl DirEntry {
    fn is_tree(&self) -> bool {
        self.filemode == 0o040000
    }
}

struct DirState {
    entries: Vec<(String, DirEntry)>,
    /// Cached tree OID for this directory; `None` means dirty (needs rebuild).
    oid: Option<git2::Oid>,
}

// ─── TreeState ────────────────────────────────────────────────────────────────

/// Tracks the full file-tree state across commits incrementally.
/// Only dirty (changed) directories are rebuilt on each `flush()`.
pub struct TreeState {
    dirs: HashMap<PathBuf, DirState>,
    /// Directories modified since the last flush, in deepest-first order.
    /// BTreeSet gives sorted iteration (longer paths sort after shorter ones with
    /// the same prefix only when the comparison works out — we re-sort by depth in flush).
    dirty: HashSet<PathBuf>,
    /// Lazy directory loader (incremental mode); `None` means all dirs were preloaded.
    src: Option<TreeSource>,
}

/// Reads tree objects for lazy loading: trees written during this run (not yet on
/// disk) are served from `mem`, everything else from the given repositories' ODBs.
pub struct TreeSource {
    mem: HashMap<git2::Oid, Vec<u8>>,
    repos: Vec<git2::Repository>,
}

impl TreeSource {
    pub fn new(repo_paths: &[&Path]) -> Result<Self> {
        let repos = repo_paths.iter()
            .map(|p| git2::Repository::open(p)
                .with_context(|| format!("opening {}", p.display())))
            .collect::<Result<Vec<_>>>()?;
        Ok(TreeSource { mem: HashMap::new(), repos })
    }

    fn read_entries(&self, oid: git2::Oid) -> Result<Vec<(String, DirEntry)>> {
        let Some(raw) = self.mem.get(&oid) else {
            for repo in &self.repos {
                if let Ok(tree) = repo.find_tree(oid) {
                    let mut entries: Vec<(String, DirEntry)> = tree.iter()
                        .filter_map(|e| e.name().ok().filter(|n| !n.is_empty()).map(|n| {
                            (n.to_string(), DirEntry { oid: e.id(), filemode: e.filemode_raw() as u32 })
                        }))
                        .collect();
                    entries.sort_unstable_by(|(na, a), (nb, b)|
                        git_tree_cmp(na, a.is_tree(), nb, b.is_tree()));
                    return Ok(entries);
                }
            }
            bail!("tree {oid} not found");
        };
        let mut entries = Vec::new();
        let mut i = 0;
        while i < raw.len() {
            let sp = raw[i..].iter().position(|&b| b == b' ')
                .context("corrupt tree: no mode terminator")? + i;
            let mode = u32::from_str_radix(std::str::from_utf8(&raw[i..sp])?, 8)?;
            let nul = raw[sp + 1..].iter().position(|&b| b == 0)
                .context("corrupt tree: no name terminator")? + sp + 1;
            let name = String::from_utf8_lossy(&raw[sp + 1..nul]).into_owned();
            let oid = git2::Oid::from_bytes(&raw[nul + 1..nul + 21])?;
            entries.push((name, DirEntry { oid, filemode: mode }));
            i = nul + 21;
        }
        Ok(entries)
    }
}

impl TreeState {
    /// Build a lazily-loading TreeState rooted at an existing tree object.
    pub fn from_tree(root_tree: git2::Oid, src: TreeSource) -> Result<Self> {
        let entries = src.read_entries(root_tree)?;
        let mut dirs = HashMap::new();
        dirs.insert(PathBuf::new(), DirState { entries, oid: Some(root_tree) });
        Ok(TreeState { dirs, dirty: HashSet::new(), src: Some(src) })
    }

    /// Re-root a lazy TreeState at another tree, keeping the loader (and the cache
    /// of trees written so far).
    pub fn reroot(&mut self, root_tree: git2::Oid) -> Result<()> {
        let src = self.src.as_ref().context("reroot needs a lazy TreeState")?;
        let entries = src.read_entries(root_tree)?;
        self.dirs.clear();
        self.dirty.clear();
        self.dirs.insert(PathBuf::new(), DirState { entries, oid: Some(root_tree) });
        Ok(())
    }

    /// Root tree OID of the current (flushed) state.
    pub fn root_oid(&self) -> Option<git2::Oid> {
        self.dirs.get(Path::new("")).and_then(|s| s.oid)
    }

    /// Make sure `dir` and all its ancestors are loaded (lazy mode only).
    fn ensure_dir(&mut self, dir: &Path) -> Result<()> {
        if self.dirs.contains_key(dir) {
            return Ok(());
        }
        let parent = dir.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
        self.ensure_dir(&parent)?;
        let name = dir.file_name().and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("bad dir path {}", dir.display()))?;
        let child = {
            let pstate = &self.dirs[&parent];
            pstate.entries.binary_search_by(|(n, e)| git_tree_cmp(n, e.is_tree(), name, true))
                .ok().map(|i| pstate.entries[i].1.oid)
        };
        let state = match (child, &self.src) {
            (Some(oid), Some(src)) => DirState { entries: src.read_entries(oid)?, oid: Some(oid) },
            _ => DirState { entries: Vec::new(), oid: None },
        };
        self.dirs.insert(dir.to_path_buf(), state);
        Ok(())
    }

    /// Build a TreeState by eagerly loading every directory (and its ancestors)
    /// touched by any blob or deletion in `patch_results` from the linus base commit.
    pub fn new(
        repo: &git2::Repository,
        base_commit_sha1: &str,
        patch_results: &[PatchResult],
    ) -> Result<Self> {
        // Collect all directories (and their ancestors) that will be modified.
        let mut all_dirs: HashSet<PathBuf> = HashSet::new();
        all_dirs.insert(PathBuf::new()); // root is always needed

        for result in patch_results {
            for blob in &result.blobs {
                ancestors_including_self(&blob.path, &mut all_dirs);
            }
            for deleted in &result.deleted {
                ancestors_including_self(deleted, &mut all_dirs);
            }
        }

        // Load each directory's initial entries from the base commit tree.
        let base_oid = git2::Oid::from_str(base_commit_sha1)?;
        let base_commit = repo.find_commit(base_oid).context("finding base commit")?;
        let base_tree_oid = base_commit.tree_id();

        let mut dirs = HashMap::with_capacity(all_dirs.len());
        for dir_path in all_dirs {
            let (entries, tree_oid) = load_dir_entries(repo, base_tree_oid, &dir_path)
                .with_context(|| format!("loading base dir '{}'", dir_path.display()))?;
            dirs.insert(dir_path, DirState { entries, oid: tree_oid });
        }

        Ok(TreeState { dirs, dirty: HashSet::new(), src: None })
    }

    /// Record a blob add/modify at `path`.
    pub fn update(&mut self, path: &Path, oid: git2::Oid, mode: u32) -> Result<()> {
        let dir = path.parent().unwrap_or_else(|| Path::new(""));
        let name = path.file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("path has no filename: {}", path.display()))?;

        self.ensure_dir(dir)?;
        let state = self.dirs.get_mut(dir)
            .ok_or_else(|| anyhow::anyhow!("directory not found: {}", dir.display()))?;
        let target_is_tree = mode == 0o040000;
        match state.entries.binary_search_by(|(n, entry)| {
            git_tree_cmp(n, entry.is_tree(), name, target_is_tree)
        }) {
            Ok(idx) => {
                state.entries[idx].1 = DirEntry { oid, filemode: mode };
            }
            Err(idx) => {
                state.entries.insert(idx, (name.to_string(), DirEntry { oid, filemode: mode }));
            }
        }
        self.mark_dirty(dir);
        Ok(())
    }

    /// Record a file deletion at `path`.
    pub fn delete(&mut self, path: &Path) -> Result<()> {
        let dir = path.parent().unwrap_or_else(|| Path::new(""));
        let name = path.file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("path has no filename: {}", path.display()))?;

        if self.src.is_some() {
            self.ensure_dir(dir)?;
        }
        if let Some(state) = self.dirs.get_mut(dir) {
            let idx = state.entries.binary_search_by(|(n, entry)| {
                git_tree_cmp(n, entry.is_tree(), name, false)
            }).or_else(|_| {
                state.entries.binary_search_by(|(n, entry)| {
                    git_tree_cmp(n, entry.is_tree(), name, true)
                })
            });
            if let Ok(idx) = idx {
                state.entries.remove(idx);
                self.mark_dirty(dir);
            }
        }
        Ok(())
    }

    /// Rebuild all dirty trees (deepest first), write them to `pack`, and return
    /// the root tree OID.  Must be called after all `update`/`delete` calls for
    /// the current commit and before moving on to the next.
    pub fn flush(&mut self, pack: &mut CommitPackWriter) -> Result<git2::Oid> {
        let dirty_set = std::mem::take(&mut self.dirty);
        // Sort deepest-first so children are rebuilt before their parents.
        let mut dirty: Vec<PathBuf> = dirty_set.into_iter().collect();
        dirty.sort_unstable_by(|a, b| {
            b.components().count().cmp(&a.components().count())
                .then_with(|| a.cmp(b))
        });

        for dir_path in &dirty {
            // Skip if somehow already clean (shouldn't normally happen)
            if self.dirs.get(dir_path).map_or(true, |s| s.oid.is_some()) {
                continue;
            }

            // Git has no empty trees: drop an emptied directory from its parent.
            if !dir_path.as_os_str().is_empty()
                && self.dirs.get(dir_path).unwrap().entries.is_empty()
            {
                if let Some(dir_name) = dir_path.file_name().and_then(|n| n.to_str()) {
                    let parent = dir_path.parent().unwrap_or_else(|| Path::new(""));
                    let parent_state = self.dirs.get_mut(parent).unwrap();
                    if let Ok(idx) = parent_state.entries.binary_search_by(|(n, entry)| {
                        git_tree_cmp(n, entry.is_tree(), dir_name, true)
                    }) {
                        parent_state.entries.remove(idx);
                    }
                    parent_state.oid = None;
                }
                continue;
            }

            // Encode the current entries of this directory
            let tree_bytes = {
                let state = self.dirs.get(dir_path).unwrap();
                encode_tree(&state.entries)
            };

            // Write the tree object with path-based delta compression
            let oid = pack.write_tree(&tree_bytes, dir_path)?;

            // Cache the fresh OID
            self.dirs.get_mut(dir_path).unwrap().oid = Some(oid);
            if let Some(src) = &mut self.src {
                src.mem.insert(oid, tree_bytes);
            }

            // Update this directory's entry in its parent
            if let Some(dir_name) = dir_path.file_name().and_then(|n| n.to_str()) {
                let parent = dir_path.parent().unwrap_or_else(|| Path::new(""));
                let parent_state = self.dirs.get_mut(parent).unwrap();
                let dir_name_string = dir_name.to_string();
                match parent_state.entries.binary_search_by(|(n, entry)| {
                    git_tree_cmp(n, entry.is_tree(), &dir_name_string, true)
                }) {
                    Ok(idx) => {
                        parent_state.entries[idx].1 = DirEntry { oid, filemode: 0o040000 };
                    }
                    Err(idx) => {
                        parent_state.entries.insert(idx, (dir_name_string, DirEntry { oid, filemode: 0o040000 }));
                    }
                }
                // Parent was already marked dirty via mark_dirty(); keep oid=None
                parent_state.oid = None;
            }
        }

        self.dirs
            .get(&PathBuf::new())
            .and_then(|s| s.oid)
            .ok_or_else(|| anyhow::anyhow!("root tree OID not available after flush"))
    }

    /// Mark `start` and all its ancestor directories as dirty (oid = None).
    /// Stops early if an ancestor is already in `dirty` (implies all higher dirs are too).
    fn mark_dirty(&mut self, start: &Path) {
        let mut current = start;
        loop {
            let already_dirty = self.dirty.contains(current);
            if let Some(state) = self.dirs.get_mut(current) {
                state.oid = None;
            }
            if already_dirty {
                return;
            }
            self.dirty.insert(current.to_path_buf());
            if current == Path::new("") {
                return;
            }
            current = current.parent().unwrap_or_else(|| Path::new(""));
        }
    }
}

// ─── Tree encoding ────────────────────────────────────────────────────────────

fn encode_tree(entries: &[(String, DirEntry)]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(entries.len() * 32);
    for (name, entry) in entries {
        buf.extend_from_slice(mode_str(entry.filemode).as_bytes());
        buf.push(b' ');
        buf.extend_from_slice(name.as_bytes());
        buf.push(0u8);
        buf.extend_from_slice(entry.oid.as_bytes());
    }
    buf
}

/// git's tree entry sort order: directories are treated as if their name ends with '/'.
fn git_tree_cmp(na: &str, a_tree: bool, nb: &str, b_tree: bool) -> Ordering {
    let a = na.as_bytes();
    let b = nb.as_bytes();
    let min_len = a.len().min(b.len());
    match a[..min_len].cmp(&b[..min_len]) {
        Ordering::Equal => {
            let ca = if a.len() == min_len { if a_tree { b'/' } else { 0u8 } } else { a[min_len] };
            let cb = if b.len() == min_len { if b_tree { b'/' } else { 0u8 } } else { b[min_len] };
            ca.cmp(&cb)
        }
        other => other,
    }
}

fn mode_str(mode: u32) -> &'static str {
    match mode {
        0o100644 => "100644",
        0o100755 => "100755",
        0o120000 => "120000",
        0o040000 => "40000", // directories: no leading zero
        _ => "100644",
    }
}

// ─── Commit encoding ──────────────────────────────────────────────────────────

/// Encode a git commit object (raw bytes, no "commit SIZE\0" header — that's added
/// by `CommitPackWriter::write_object()` when computing the SHA-1 and pack entry).
pub fn encode_commit(
    tree_oid: git2::Oid,
    parent_oid: Option<git2::Oid>,
    meta: &PatchMeta,
    committer_timestamp: i64,
) -> Vec<u8> {
    use std::fmt::Write;
    let mut s = String::with_capacity(512);

    writeln!(s, "tree {tree_oid}").unwrap();
    if let Some(p) = parent_oid {
        writeln!(s, "parent {p}").unwrap();
    }

    // Sanitize name and email (fast-import forbids '<' and '>') directly into s
    s.push_str("author ");
    if meta.author_name.is_empty() {
        s.push_str("Unknown");
    } else if meta.author_name.contains('<') || meta.author_name.contains('>') {
        s.extend(meta.author_name.chars().filter(|&c| c != '<' && c != '>'));
    } else {
        s.push_str(&meta.author_name);
    }
    s.push_str(" <");
    if meta.author_email.is_empty() {
        s.push_str("unknown@unknown");
    } else if meta.author_email.contains('<') || meta.author_email.contains('>') {
        s.extend(meta.author_email.chars().filter(|&c| c != '<' && c != '>'));
    } else {
        s.push_str(&meta.author_email);
    }
    writeln!(s, "> {} {}", meta.author_timestamp, meta.author_tz).unwrap();
    writeln!(s, "committer kexpand <kexpand@localhost> {committer_timestamp} +0000").unwrap();
    writeln!(s).unwrap();

    // Commit message body (same logic as the old fast_import::build_message)
    let subject = if meta.subject.is_empty() {
        meta.source_path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string()
    } else {
        meta.subject.clone()
    };
    write!(s, "{subject}").unwrap();

    if !meta.body.is_empty() {
        write!(s, "\n\n{}", meta.body).unwrap();
    }

    let mut extras = String::new();
    if !meta.references.is_empty() {
        extras.push_str(&format!("References: {}\n", meta.references));
    }
    if !meta.patch_mainline.is_empty() {
        extras.push_str(&format!("Patch-mainline: {}\n", meta.patch_mainline));
    }
    if !meta.git_commit.is_empty() {
        extras.push_str(&format!("Git-commit: {}\n", meta.git_commit));
    }
    if !meta.git_repo.is_empty() {
        extras.push_str(&format!("Git-repo: {}\n", meta.git_repo));
    }
    if !meta.alt_commit.is_empty() {
        extras.push_str(&format!("Alt-commit: {}\n", meta.alt_commit));
    }
    if !extras.is_empty() {
        write!(s, "\n\n{extras}").unwrap();
    }

    let patch_name = meta.source_path.to_str().unwrap_or("");
    if !patch_name.is_empty() {
        write!(s, "\nOriginal-patch: {patch_name}\n").unwrap();
    }

    if !s.ends_with('\n') {
        s.push('\n');
    }

    s.into_bytes()
}

/// Encode a commit that mirrors a kernel-source commit: `header_lines` are the raw
/// author/committer (and encoding) header lines copied verbatim, `message` is the
/// original message; a `suse-commit:` trailer referencing `ks_commit` is appended.
pub fn encode_commit_raw(
    tree_oid: git2::Oid,
    parents: &[git2::Oid],
    header_lines: &[u8],
    message: &[u8],
    ks_commit: git2::Oid,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(header_lines.len() + message.len() + 200);
    out.extend_from_slice(format!("tree {tree_oid}\n").as_bytes());
    for p in parents {
        out.extend_from_slice(format!("parent {p}\n").as_bytes());
    }
    out.extend_from_slice(header_lines);
    out.extend_from_slice(b"\n");
    let end = message.iter().rposition(|&b| b != b'\n').map_or(0, |i| i + 1);
    out.extend_from_slice(&message[..end]);
    out.extend_from_slice(format!("\n\nsuse-commit: {ks_commit}\n").as_bytes());
    out
}

// ─── Base-tree loading helpers ────────────────────────────────────────────────

/// Load the entries of the directory at `dir_path` from the base commit tree.
/// Returns `(entries, Some(tree_oid))` if the directory exists, or
/// `(empty, None)` if it does not exist in the base tree.
fn load_dir_entries(
    repo: &git2::Repository,
    base_tree_oid: git2::Oid,
    dir_path: &Path,
) -> Result<(Vec<(String, DirEntry)>, Option<git2::Oid>)> {
    let mut current_oid = base_tree_oid;

    // Walk down the tree one component at a time
    for component in dir_path.components() {
        let name = match component {
            Component::Normal(n) => n.to_str().unwrap_or(""),
            _ => continue,
        };
        let tree = repo.find_tree(current_oid)?;
        // Extract OID before tree is dropped (entry borrows tree)
        let child_oid = tree.get_name(name)
            .filter(|e| e.kind() == Some(git2::ObjectType::Tree))
            .map(|e| e.id());
        match child_oid {
            Some(oid) => current_oid = oid,
            None => return Ok((Vec::new(), None)), // dir absent from base
        }
    }

    let tree = repo.find_tree(current_oid)?;
    let tree_oid = Some(tree.id());
    let mut entries = Vec::with_capacity(tree.len());
    for entry in tree.iter() {
        let name = match entry.name() {
            Ok(n) if !n.is_empty() => n.to_string(),
            _ => continue,
        };
        entries.push((name, DirEntry {
            oid: entry.id(),
            filemode: entry.filemode_raw() as u32,
        }));
    }
    entries.sort_unstable_by(|(na, a), (nb, b)| git_tree_cmp(na, a.is_tree(), nb, b.is_tree()));
    Ok((entries, tree_oid))
}

// ─── Path helpers ─────────────────────────────────────────────────────────────

/// Collect the directory of `path` and all its ancestor directories into `set`.
fn ancestors_including_self(path: &Path, set: &mut HashSet<PathBuf>) {
    if let Some(parent) = path.parent() {
        let mut current = if parent.as_os_str().is_empty() {
            PathBuf::new()
        } else {
            parent.to_path_buf()
        };
        loop {
            let new = set.insert(current.clone());
            if !new || current == PathBuf::new() {
                break;
            }
            current = match current.parent() {
                Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
                _ => PathBuf::new(),
            };
        }
    } else {
        set.insert(PathBuf::new());
    }
}

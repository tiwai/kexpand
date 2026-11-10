//! Incremental expansion: create one commit per kernel-source commit on top of an
//! existing expanded repository (`--update`).
//!
//! Patch application is per-file: the expanded content of a file depends only on its
//! base content and the ordered patches touching it.  For a kernel-source commit C
//! with (first expandable) parent P we therefore start from P's expanded tree and
//! recompute only the files touched by patches that were added, removed or changed
//! between series(P) and series(C).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{bail, Context, Result};
use git2::{Oid, Repository};
use libpatch::analysis::AnalysisSet;
use libpatch::modified_file::ModifiedFile;
use rayon::prelude::*;

use crate::apply::{apply_file_patches, collect_and_hash, parse_patches, resolve_paths, ParsedPatch};
use crate::arena::ByteArena;
use crate::blob_shards::BlobShardPool;
use crate::commit_pack::CommitPackWriter;
use crate::git_source::GitSource;
use crate::kernel_source::parse_base_tag;
use crate::series::parse_series;
use crate::tree_state::{encode_commit_raw, TreeSource, TreeState};

pub struct Params {
    pub output: PathBuf,
    pub linux: PathBuf,
    pub branch: String,
    pub base: Option<String>,
    pub rev: Option<String>,
    pub force: bool,
    pub max_fuzz: usize,
    pub skip_failed: bool,
    pub verbose: bool,
    pub quiet: bool,
    pub no_repack: bool,
    pub n_shards: usize,
}

/// What we know about the expanded counterpart of a kernel-source commit.
#[derive(Clone, Copy)]
struct Expanded {
    commit: Oid,
    tree: Oid,
}

struct SeriesIndex {
    paths: Vec<PathBuf>,
    idx: HashMap<PathBuf, usize>,
}

struct SeriesState {
    base_tag: String,
    index: Rc<SeriesIndex>,
    /// Patch blob OID per series entry (parallel to `index.paths`).
    oids: Vec<Oid>,
}

struct ParsedEntry<'a> {
    patch: ParsedPatch<'a>,
    /// All file lookup keys touched (including rename targets).
    keys: Vec<PathBuf>,
    renames: Vec<(PathBuf, PathBuf)>,
}

struct Ctx<'a, 'k> {
    ks: &'k Repository,
    arena: &'a ByteArena<'a>,
    series_cache: HashMap<Oid, Rc<SeriesIndex>>,
    parsed: HashMap<Oid, ParsedEntry<'a>>,
    /// file key -> series paths of patches (any version) touching it
    touch_index: HashMap<PathBuf, HashSet<PathBuf>>,
}

fn is_expandable(tree: &git2::Tree) -> bool {
    tree.get_path(Path::new("series.conf")).is_ok()
        && tree.get_path(Path::new("rpm/config.sh")).is_ok()
}

fn suse_commit_of(msg: &str) -> Option<Oid> {
    msg.lines().rev()
        .find_map(|l| l.strip_prefix("suse-commit: "))
        .and_then(|h| Oid::from_str(h.trim()).ok())
}

impl<'a, 'k> Ctx<'a, 'k> {
    fn blob_of(&self, tree: &git2::Tree, path: &Path) -> Result<git2::Blob<'_>> {
        let e = tree.get_path(path).with_context(|| format!("{} not found", path.display()))?;
        self.ks.find_blob(e.id()).with_context(|| format!("reading {}", path.display()))
    }

    /// Build the series state for `tree`, reusing `parent` data where unchanged.
    /// Returns the state and the indices of entries that are new relative to `parent`.
    fn build_series(
        &mut self,
        tree: &git2::Tree,
        parent: Option<(&git2::Tree, &SeriesState)>,
    ) -> Result<(SeriesState, Vec<usize>)> {
        let sc_oid = tree.get_path(Path::new("series.conf"))?.id();
        let index = match self.series_cache.get(&sc_oid) {
            Some(i) => i.clone(),
            None => {
                let blob = self.ks.find_blob(sc_oid)?;
                let paths = parse_series(blob.content())?;
                let idx = paths.iter().cloned().enumerate().map(|(i, p)| (p, i)).collect();
                let i = Rc::new(SeriesIndex { paths, idx });
                self.series_cache.insert(sc_oid, i.clone());
                i
            }
        };
        let base_tag = parse_base_tag(self.blob_of(tree, Path::new("rpm/config.sh"))?.content())?;

        let zero = Oid::ZERO_SHA1;
        let mut changed: HashMap<PathBuf, Oid> = HashMap::new();
        if let Some((ptree, _)) = parent {
            let diff = self.ks.diff_tree_to_tree(Some(ptree), Some(tree), None)?;
            for d in diff.deltas() {
                if let Some(p) = d.new_file().path() {
                    if index.idx.contains_key(p) {
                        changed.insert(p.to_path_buf(), d.new_file().id());
                    }
                }
            }
            // Entries of the old series that vanished from the new one don't matter here.
        }

        let mut fresh = Vec::new();
        let oids: Vec<Oid> = match parent {
            Some((_, ps)) if Rc::ptr_eq(&ps.index, &index) => {
                let mut o = ps.oids.clone();
                for (p, id) in &changed {
                    let i = index.idx[p];
                    if o[i] != *id { o[i] = *id; fresh.push(i); }
                }
                o
            }
            _ => {
                // Directory trees can be huge (patches.suse), so read each only once.
                let mut dirs: HashMap<PathBuf, HashMap<Vec<u8>, Oid>> = HashMap::new();
                let mut o = Vec::with_capacity(index.paths.len());
                for (i, p) in index.paths.iter().enumerate() {
                    let reuse = parent.and_then(|(_, ps)| {
                        if changed.contains_key(p) { return None; }
                        ps.index.idx.get(p).map(|&j| ps.oids[j])
                    });
                    match reuse {
                        Some(id) => o.push(id),
                        None => {
                            let id = match changed.get(p) {
                                Some(id) => *id,
                                None => {
                                    let dir = p.parent().unwrap_or(Path::new("")).to_path_buf();
                                    if !dirs.contains_key(&dir) {
                                        let t = if dir.as_os_str().is_empty() {
                                            tree.clone()
                                        } else {
                                            self.ks.find_tree(tree.get_path(&dir)
                                                .with_context(|| format!("{} not found", dir.display()))?.id())?
                                        };
                                        dirs.insert(dir.clone(), t.iter()
                                            .map(|e| (e.name_bytes().to_vec(), e.id())).collect());
                                    }
                                    let name = p.file_name().map(|n| n.as_encoded_bytes()).unwrap_or(b"");
                                    *dirs[&dir].get(name).with_context(||
                                        format!("patch {} listed in series.conf is missing", p.display()))?
                                }
                            };
                            o.push(id);
                            fresh.push(i);
                        }
                    }
                }
                o
            }
        };
        if let Some(i) = oids.iter().position(|o| *o == zero) {
            bail!("patch {} listed in series.conf is missing", index.paths[i].display());
        }
        Ok((SeriesState { base_tag, index, oids }, fresh))
    }

    /// Parse (in parallel) all not-yet-parsed patches among `entries` and register them
    /// in the touch index.
    fn register(&mut self, state: &SeriesState, fresh: &[usize]) -> Result<()> {
        let mut seen = HashSet::new();
        let missing: Vec<(PathBuf, Oid)> = fresh.iter()
            .map(|&i| (state.index.paths[i].clone(), state.oids[i]))
            .filter(|(_, o)| !self.parsed.contains_key(o) && seen.insert(*o))
            .collect();
        if !missing.is_empty() {
            let ks_path = self.ks.path().to_path_buf();
            let blobs: Vec<(PathBuf, Vec<u8>)> = missing.par_iter()
                .map_init(
                    || Repository::open(&ks_path).ok(),
                    |repo, (p, oid)| -> Result<(PathBuf, Vec<u8>)> {
                        let repo = repo.as_ref().context("opening kernel-source repo")?;
                        let blob = repo.find_blob(*oid)
                            .with_context(|| format!("reading patch {}", p.display()))?;
                        Ok((p.clone(), blob.content().to_vec()))
                    })
                .collect::<Result<_>>()?;
            let parsed = parse_patches(blobs, self.arena)?;
            for ((_, oid), pp) in missing.iter().zip(parsed) {
                let mut keys = Vec::new();
                let mut renames = Vec::new();
                if let Some(tp) = &pp.text_patch {
                    for fp in &tp.file_patches {
                        let (lookup, target) = resolve_paths(fp);
                        keys.push(lookup.clone());
                        if fp.is_rename() {
                            keys.push(target.clone());
                            renames.push((lookup, target));
                        }
                    }
                }
                self.parsed.insert(*oid, ParsedEntry { patch: pp, keys, renames });
            }
        }
        for &i in fresh {
            let path = &state.index.paths[i];
            for k in &self.parsed[&state.oids[i]].keys {
                self.touch_index.entry(k.clone()).or_default().insert(path.clone());
            }
        }
        Ok(())
    }

    /// Series positions (ascending) of patches in `state` that may touch files in `s`.
    fn candidates(&self, state: &SeriesState, s: &HashSet<PathBuf>) -> BTreeSet<usize> {
        let mut out = BTreeSet::new();
        for f in s {
            if let Some(ps) = self.touch_index.get(f) {
                for p in ps {
                    if let Some(&i) = state.index.idx.get(p) {
                        out.insert(i);
                    }
                }
            }
        }
        out
    }

    /// Set of files whose expanded content may differ between series(P) and series(C).
    fn affected_files(&self, c: &SeriesState, p: Option<&SeriesState>) -> HashSet<PathBuf> {
        let mut s: HashSet<PathBuf> = HashSet::new();
        let add = |oid: &Oid, s: &mut HashSet<PathBuf>| {
            s.extend(self.parsed[oid].keys.iter().cloned());
        };
        match p {
            None => {
                for o in &c.oids { add(o, &mut s); }
                return s;
            }
            Some(p) if Rc::ptr_eq(&p.index, &c.index) => {
                for (a, b) in p.oids.iter().zip(&c.oids) {
                    if a != b { add(a, &mut s); add(b, &mut s); }
                }
            }
            Some(p) => {
                let pp: Vec<(&PathBuf, &Oid)> = p.index.paths.iter().zip(&p.oids).collect();
                let cc: Vec<(&PathBuf, &Oid)> = c.index.paths.iter().zip(&c.oids).collect();
                let pset: HashSet<_> = pp.iter().copied().collect();
                let cset: HashSet<_> = cc.iter().copied().collect();
                for (_, o) in pp.iter().filter(|e| !cset.contains(*e)) { add(o, &mut s); }
                for (_, o) in cc.iter().filter(|e| !pset.contains(*e)) { add(o, &mut s); }
                // Reordered common patches: everything inside the mismatching window.
                let cp: Vec<_> = pp.iter().filter(|e| cset.contains(*e)).collect();
                let cc2: Vec<_> = cc.iter().filter(|e| pset.contains(*e)).collect();
                if cp != cc2 {
                    let first = cp.iter().zip(&cc2).position(|(a, b)| a != b).unwrap_or(0);
                    let tail = cp.iter().rev().zip(cc2.iter().rev()).position(|(a, b)| a != b).unwrap_or(0);
                    for e in &cp[first..cp.len() - tail] { add(e.1, &mut s); }
                    for e in &cc2[first..cc2.len() - tail] { add(e.1, &mut s); }
                }
            }
        }
        // Close over rename links.
        loop {
            let mut grew = false;
            for i in self.candidates(c, &s) {
                for (a, b) in &self.parsed[&c.oids[i]].renames {
                    if s.contains(a) != s.contains(b) {
                        s.insert(a.clone());
                        s.insert(b.clone());
                        grew = true;
                    }
                }
            }
            if !grew { break; }
        }
        s
    }
}

pub fn run(p: &Params) -> Result<()> {
    let ks = Repository::open(".").context("opening kernel-source repo (run in or use -C)")?;
    let out = Repository::open(&p.output)
        .with_context(|| format!("opening output repo {} (must already exist)", p.output.display()))?;

    // Resolve base commit in the expanded repo and its kernel-source counterpart.
    let base_spec = p.base.clone().unwrap_or_else(|| format!("refs/heads/{}", p.branch));
    let base_commit = out.revparse_single(&base_spec)
        .with_context(|| format!("resolving base '{base_spec}' in output repo"))?
        .peel_to_commit()?;
    let ks_base = suse_commit_of(base_commit.message().unwrap_or(""))
        .context("base commit has no 'suse-commit:' tag")?;
    let target = ks.revparse_single(p.rev.as_deref().unwrap_or("HEAD"))
        .context("resolving kernel-source target revision")?
        .peel_to_commit()?;

    // Commits to expand, parents first.
    let mut walk = ks.revwalk()?;
    walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;
    walk.push(target.id())?;
    walk.hide(ks_base)
        .with_context(|| format!("kernel-source commit {ks_base} (from base) not found"))?;
    let order: Vec<Oid> = walk.collect::<std::result::Result<_, _>>()?;
    if !p.quiet { println!("Base: {} (suse-commit {}), {} commits to expand", base_commit.id(), ks_base, order.len()); }
    if order.is_empty() {
        println!("Already up to date");
        return Ok(());
    }
    let in_range: HashSet<Oid> = order.iter().copied().collect();

    // Map kernel-source commits outside the range (boundary parents) to expanded ones.
    let mut exp: HashMap<Oid, Option<Expanded>> = HashMap::new();
    exp.insert(ks_base, Some(Expanded { commit: base_commit.id(), tree: base_commit.tree_id() }));
    let mut pending: HashMap<Oid, usize> = HashMap::new();
    let mut needed: HashSet<Oid> = HashSet::new();
    for &c in &order {
        for par in ks.find_commit(c)?.parent_ids() {
            *pending.entry(par).or_default() += 1;
            if !in_range.contains(&par) && !exp.contains_key(&par) {
                if is_expandable(&ks.find_commit(par)?.tree()?) {
                    needed.insert(par);
                } else {
                    exp.insert(par, None);
                }
            }
        }
    }
    if !needed.is_empty() {
        if !p.quiet { println!("Locating {} boundary parent(s) in output history...", needed.len()); }
        // The expanded history mirrors the kernel-source DAG, so walk both in lockstep
        // (newest first) from the base: the parents of a mapped commit correspond, in
        // order, to the parents of its expanded counterpart (minus unexpandable ones).
        let mut mapped: HashMap<Oid, Oid> = HashMap::new();
        mapped.insert(ks_base, base_commit.id());
        let mut heap = std::collections::BinaryHeap::new();
        heap.push((ks.find_commit(ks_base)?.time().seconds(), ks_base));
        let mut visited: HashSet<Oid> = HashSet::new();
        while let Some((_, x)) = heap.pop() {
            if !visited.insert(x) { continue; }
            if needed.is_empty() { break; }
            let e = out.find_commit(mapped[&x])?;
            let kx = ks.find_commit(x)?;
            let mut kp: Vec<Oid> = kx.parent_ids().collect();
            let ep: Vec<Oid> = e.parent_ids().collect();
            if kp.len() != ep.len() {
                let mut f = Vec::new();
                for q in kp {
                    if is_expandable(&ks.find_commit(q)?.tree()?) { f.push(q); }
                }
                kp = f;
            }
            if kp.len() != ep.len() {
                bail!("cannot map parents of kernel-source commit {x} to expanded commit {}", e.id());
            }
            for (q, eq) in kp.into_iter().zip(ep) {
                if mapped.insert(q, eq).is_none() {
                    heap.push((ks.find_commit(q)?.time().seconds(), q));
                }
                if needed.remove(&q) {
                    let c = out.find_commit(eq)?;
                    if suse_commit_of(c.message().unwrap_or("")) != Some(q) {
                        bail!("expanded commit {eq} does not carry suse-commit {q}");
                    }
                    exp.insert(q, Some(Expanded { commit: eq, tree: c.tree_id() }));
                }
            }
        }
        if let Some(k) = needed.iter().next() {
            bail!("kernel-source parent {k} not found in output history (not expanded yet?)");
        }
    }

    let arena = ByteArena::new();
    let mut ctx = Ctx {
        ks: &ks,
        arena: &arena,
        series_cache: HashMap::new(),
        parsed: HashMap::new(),
        touch_index: HashMap::new(),
    };

    let out_path = out.path().to_path_buf();
    let pack_dir = out_path.join("objects/pack");
    let packs_before = list_idx(&pack_dir)?;
    let mut shard_pool = BlobShardPool::new(&out_path, p.n_shards)?;
    let mut pack = CommitPackWriter::create(&pack_dir)?;
    let src = TreeSource::new(&[&out_path, &p.linux])?;
    let mut tree_state = TreeState::from_tree(base_commit.tree_id(), src)?;
    let odb = out.odb()?;
    let analyses = AnalysisSet::default();
    let mut git_sources: HashMap<String, GitSource> = HashMap::new();
    let mut states: HashMap<Oid, Rc<SeriesState>> = HashMap::new();
    let mut blob_seen: HashSet<Oid> = HashSet::new();
    let mut content_buf: Vec<u8> = Vec::with_capacity(256 * 1024);
    let mut n_done = 0usize;
    let mut n_files = 0usize;

    for &c in &order {
        let kc = ks.find_commit(c)?;
        let ktree = kc.tree()?;
        let parents: Vec<Oid> = kc.parent_ids().collect();

        if is_expandable(&ktree) {
            // Expanded parents (deduplicated, in order) and the one we build upon.
            let mut exp_parents: Vec<Oid> = Vec::new();
            let mut base_ks: Option<(Oid, Expanded)> = None;
            for par in &parents {
                if let Some(Some(e)) = exp.get(par) {
                    if base_ks.is_none() { base_ks = Some((*par, *e)); }
                    if !exp_parents.contains(&e.commit) { exp_parents.push(e.commit); }
                }
            }

            // Series state of the parent we build upon (from scratch if not cached).
            let parent_state: Option<(Rc<SeriesState>, git2::Tree)> = match &base_ks {
                Some((kp, _)) => {
                    let ptree = ks.find_commit(*kp)?.tree()?;
                    let st = match states.get(kp) {
                        Some(s) => s.clone(),
                        None => {
                            let (s, fresh) = ctx.build_series(&ptree, None)?;
                            ctx.register(&s, &fresh)?;
                            let s = Rc::new(s);
                            states.insert(*kp, s.clone());
                            s
                        }
                    };
                    Some((st, ptree))
                }
                None => None,
            };
            let (state_c, fresh) = ctx.build_series(
                &ktree, parent_state.as_ref().map(|(s, t)| (t, &**s)))?;
            ctx.register(&state_c, &fresh)?;

            // Starting tree and affected file set.
            if !git_sources.contains_key(&state_c.base_tag) {
                git_sources.insert(state_c.base_tag.clone(), GitSource::new(&p.linux, &state_c.base_tag)?);
            }
            let same_base = parent_state.as_ref().map_or(false, |(s, _)| s.base_tag == state_c.base_tag);
            let (start_tree, s) = if same_base {
                let (ps, _) = parent_state.as_ref().unwrap();
                (base_ks.unwrap().1.tree, ctx.affected_files(&state_c, Some(ps)))
            } else {
                (git_sources[&state_c.base_tag].tree_oid(), ctx.affected_files(&state_c, None))
            };
            if tree_state.root_oid() != Some(start_tree) {
                tree_state.reroot(start_tree)?;
            }

            if !s.is_empty() {
                // Base contents of affected files, then apply the relevant patches in order.
                let paths: Vec<PathBuf> = s.iter().cloned().collect();
                let prewarmed = git_sources[&state_c.base_tag].prewarm_files(&paths)
                    .context("loading base files")?;
                let mut files: HashMap<PathBuf, ModifiedFile> = HashMap::new();
                let mut modes: HashMap<PathBuf, u32> = HashMap::new();
                for (path, content, mode) in prewarmed {
                    let mf = match content {
                        Some(b) => ModifiedFile::new(arena.alloc(b), true, None),
                        None => ModifiedFile::new_non_existent(),
                    };
                    files.insert(path.clone(), mf);
                    modes.insert(path, mode);
                }
                let mut touched = HashSet::new();
                let mut renamed = HashSet::new();
                for i in ctx.candidates(&state_c, &s) {
                    let entry = &ctx.parsed[&state_c.oids[i]];
                    let Some(tp) = &entry.patch.text_patch else {
                        if p.verbose { eprintln!("SKIP (binary): {}", state_c.index.paths[i].display()); }
                        continue;
                    };
                    apply_file_patches(tp, &entry.patch.meta, &mut files, Some(&s),
                        &mut touched, &mut renamed, p.max_fuzz, p.skip_failed, &analyses)
                        .with_context(|| format!("expanding kernel-source commit {c}"))?;
                }

                let mut sorted: Vec<&PathBuf> = s.iter().collect();
                sorted.sort();
                for path in sorted {
                    match files.get(path) {
                        Some(mf) if !mf.deleted => {
                            let mode = match &mf.permissions {
                                Some(pm) => {
                                    use std::os::unix::fs::PermissionsExt;
                                    pm.mode()
                                }
                                None => modes.get(path).copied().unwrap_or(0o100644),
                            };
                            let mode = if mode & 0o111 != 0 { 0o100755 } else { 0o100644 };
                            let oid = collect_and_hash(mf, &mut content_buf);
                            if blob_seen.insert(oid) && !odb.exists(oid) {
                                shard_pool.emit_blob(content_buf.clone())
                                    .context("emitting blob to shard")?;
                            }
                            tree_state.update(path, oid, mode)?;
                            n_files += 1;
                        }
                        _ => tree_state.delete(path)?,
                    }
                }
            }
            let tree_oid = tree_state.flush(&mut pack)
                .with_context(|| format!("flushing tree for kernel-source commit {c}"))?;

            // Commit mirroring the kernel-source commit.
            let mut hdr = Vec::new();
            for line in kc.raw_header_bytes().split(|&b| b == b'\n') {
                if line.starts_with(b"author ") || line.starts_with(b"committer ")
                    || line.starts_with(b"encoding ")
                {
                    hdr.extend_from_slice(line);
                    hdr.push(b'\n');
                }
            }
            let bytes = encode_commit_raw(tree_oid, &exp_parents, &hdr, kc.message_bytes(), c);
            let commit_oid = pack.write_object(1, &bytes)?;
            exp.insert(c, Some(Expanded { commit: commit_oid, tree: tree_oid }));
            if pending.get(&c).copied().unwrap_or(0) > 0 {
                states.insert(c, Rc::new(state_c));
            }
        } else {
            exp.insert(c, None);
        }

        for par in &parents {
            if let Some(n) = pending.get_mut(par) {
                *n -= 1;
                if *n == 0 { states.remove(par); }
            }
        }
        n_done += 1;
        if !p.quiet && n_done % 100 == 0 {
            eprintln!("  {n_done}/{} commits", order.len());
        }
    }

    // The tip must be the target's expanded commit.
    let tip = match exp.get(&target.id()) {
        Some(Some(e)) => e.commit,
        _ => bail!("target commit {} is not expandable (no series.conf)", target.id()),
    };

    shard_pool.finish().context("draining blob shards")?;
    pack.finish().context("finalizing commit pack")?;

    // Update the branch only after every object is on disk.
    let out = Repository::open(&p.output)?;
    let refname = format!("refs/heads/{}", p.branch);
    if let Ok(old) = out.refname_to_id(&refname) {
        if old != tip && !out.graph_descendant_of(tip, old)? && !p.force {
            bail!("{refname} ({old}) is not an ancestor of the new tip {tip}; use --force");
        }
    }
    out.reference(&refname, tip, true, "kexpand --update")?;
    if !p.quiet {
        println!("{n_done} kernel-source commits processed, {n_files} files recomputed; {refname} -> {tip}");
    }

    if !p.no_repack {
        let new_packs: HashSet<PathBuf> =
            list_idx(&pack_dir)?.difference(&packs_before).cloned().collect();
        if new_packs.len() > 1 {
            crate::run_repack(&pack_dir, Some(&new_packs))?;
        }
    }
    Ok(())
}

fn list_idx(pack_dir: &Path) -> Result<HashSet<PathBuf>> {
    let mut set = HashSet::new();
    if pack_dir.exists() {
        for e in std::fs::read_dir(pack_dir)? {
            let path = e?.path();
            if path.extension().map_or(false, |x| x == "idx") {
                set.insert(path);
            }
        }
    }
    Ok(set)
}

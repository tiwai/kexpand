use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use libpatch::analysis::{AnalysisSet, fn_analysis_note_noop};
use libpatch::modified_file::ModifiedFile;
use libpatch::patch::unified::parser::{parse_patch, ParseError};
use libpatch::patch::{PatchDirection, TextFilePatch, TextPatch};

use crate::arena::ByteArena;
use crate::blob_shards::BlobShardPool;
use crate::patch_header::{parse_header, PatchMeta};

#[derive(Clone)]
pub struct BlobEntry {
    pub path: PathBuf,
    /// SHA-1 of the blob content (computed at apply time via OpenSSL SHA-NI).
    /// Valid in both the alternates-ODB and shard-fast-import cases.
    pub oid: git2::Oid,
    pub mode: u32,
}

#[derive(Clone)]
pub struct PatchResult {
    pub meta: PatchMeta,
    pub blobs: Vec<BlobEntry>,
    pub deleted: Vec<PathBuf>,
}

pub struct ParsedPatch<'a> {
    pub meta: PatchMeta,
    pub text_patch: Option<TextPatch<'a>>,  // None = binary or parse error we skip
}

pub struct Stats {
    pub applied: usize,
    pub skipped_binary: usize,
    pub skipped_error: usize,
}

static TIME_COLLECT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TIME_HASH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Fill `buf` with the file content and return the git blob SHA-1.
///
/// `buf` is a caller-supplied reusable Vec cleared and refilled each call.
/// This avoids per-blob allocation for the ~55 % of blobs that are already
/// in the linux alternates ODB — only shard-bound blobs need a clone.
///
/// SHA-1 is computed via a single OpenSSL SHA-NI update on the contiguous
/// buffer (more efficient than per-line updates for typical kernel line sizes).
fn collect_and_hash(mf: &ModifiedFile, buf: &mut Vec<u8>) -> git2::Oid {
    use openssl::sha::Sha1;
    use std::sync::atomic::Ordering;

    let t0 = std::time::Instant::now();
    // Pre-calculate exact total length and reserve to avoid incremental reallocations and copies.
    let total_len: usize = mf.content.iter().map(|line| line.len()).sum();
    buf.clear();
    buf.reserve(total_len);
    unsafe {
        let mut ptr = buf.as_mut_ptr();
        for line in &mf.content {
            std::ptr::copy_nonoverlapping(line.as_ptr(), ptr, line.len());
            ptr = ptr.add(line.len());
        }
        buf.set_len(total_len);
    }
    TIME_COLLECT.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);

    let t1 = std::time::Instant::now();
    // Hash blob header + full buffer in two calls.
    let mut hasher = Sha1::new();
    let mut hdr = [0u8; 26];
    hdr[..5].copy_from_slice(b"blob ");
    let mut n = buf.len();
    let mut digits = [0u8; 20];
    let mut nd = 0;
    loop {
        digits[nd] = b'0' + (n % 10) as u8;
        nd += 1;
        n /= 10;
        if n == 0 { break; }
    }
    for i in 0..nd { hdr[5 + i] = digits[nd - 1 - i]; }
    hdr[5 + nd] = 0;
    hasher.update(&hdr[..6 + nd]);
    hasher.update(buf);

    let oid = git2::Oid::from_bytes(&hasher.finish()).expect("OpenSSL SHA-1 is 20 bytes");
    TIME_HASH.fetch_add(t1.elapsed().as_nanos() as u64, Ordering::Relaxed);
    oid
}

/// Parse all patch files in parallel. `entries` is consumed (bytes moved into arena).
pub fn parse_patches<'a>(
    entries: Vec<(PathBuf, Vec<u8>)>,
    arena: &'a ByteArena<'a>,
) -> Result<Vec<ParsedPatch<'a>>> {
    use rayon::prelude::*;

    let results: Vec<Result<ParsedPatch<'a>>> = entries
        .into_par_iter()
        .map(|(rel_path, bytes)| {
            let slice: &'a [u8] = arena.alloc(bytes);
            match parse_patch(slice, 1) {
                Ok(tp) => {
                    let meta = parse_header(tp.header, rel_path.clone());
                    Ok(ParsedPatch { meta, text_patch: Some(tp) })
                }
                Err(ParseError::UnsupportedMetadata(ref m)) if m.contains("binary") || m.contains("GIT binary") => {
                    let meta = parse_header(&[], rel_path.clone());
                    Ok(ParsedPatch { meta, text_patch: None })
                }
                Err(e) => bail!("parse error in {}: {}", rel_path.display(), e),
            }
        })
        .collect();

    results.into_iter().collect()
}

/// Collect all unique file lookup-keys referenced by the parsed patches.
/// These are the paths that need to be pre-loaded from the base git tree.
pub fn collect_patch_paths(parsed: &[ParsedPatch]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    for pp in parsed {
        if let Some(tp) = &pp.text_patch {
            for fp in &tp.file_patches {
                let (lookup_key, _) = resolve_paths(fp);
                seen.insert(lookup_key);
            }
        }
    }
    seen.into_iter().collect()
}

struct PatchTask<'a> {
    meta: PatchMeta,
    file_tasks: Vec<FileTask<'a>>,
    deleted_paths: Vec<PathBuf>,
}

struct FileTask<'a> {
    target_path: PathBuf,
    content: Vec<&'a [u8]>,
    permissions_mode: Option<u32>,
    default_mode: u32,
}

struct HashedBlob {
    path: PathBuf,
    oid: git2::Oid,
    mode: u32,
    content: Vec<u8>,
}

struct HashedPatch {
    meta: PatchMeta,
    blobs: Vec<HashedBlob>,
    deleted: Vec<PathBuf>,
}

/// Apply all parsed patches serially, emitting blobs to `shard_pool`.
/// Returns a `PatchResult` per applied patch (for later commit emission)
/// and aggregate statistics.
pub fn apply_blobs<'a>(
    parsed: Vec<ParsedPatch<'a>>,
    mut modified_files: HashMap<PathBuf, ModifiedFile<'a>>,
    mode_map: HashMap<PathBuf, u32>,
    git_odb: &git2::Odb,
    shard_pool: &mut BlobShardPool,
    max_fuzz: usize,
    skip_failed: bool,
    verbose: bool,
    quiet: bool,
) -> Result<(Vec<PatchResult>, Stats)> {
    use rayon::prelude::*;

    let mut stats = Stats { applied: 0, skipped_binary: 0, skipped_error: 0 };
    let analyses = AnalysisSet::default();

    let mut results = Vec::with_capacity(parsed.len());
    // Create 256 sharded seen sets to eliminate parallel lock contention completely.
    let blob_seen: Vec<std::sync::Mutex<HashSet<git2::Oid>>> = (0..256)
        .map(|_| std::sync::Mutex::new(HashSet::new()))
        .collect();
    let mut n_alternates: usize = 0;
    let mut n_deduped: usize = 0;

    let mut t_hash_collect = std::time::Duration::ZERO;
    let mut t_odb = std::time::Duration::ZERO;
    let mut t_shard = std::time::Duration::ZERO;

    let chunk_size = 4096;
    let mut parsed_iter = parsed.into_iter();

    loop {
        let mut chunk = Vec::with_capacity(chunk_size);
        for _ in 0..chunk_size {
            if let Some(pp) = parsed_iter.next() {
                chunk.push(pp);
            } else {
                break;
            }
        }
        if chunk.is_empty() {
            break;
        }

        // 1. Sequential Application on this chunk
        let mut patch_tasks: Vec<PatchTask> = Vec::with_capacity(chunk.len());
        for pp in chunk {
            let text_patch = match pp.text_patch {
                None => {
                    eprintln!("SKIP (binary): {}", pp.meta.source_path.display());
                    stats.skipped_binary += 1;
                    continue;
                }
                Some(tp) => tp,
            };

            if verbose {
                println!("  applying: {}", pp.meta.source_path.display());
            }

            let mut deleted_paths: Vec<PathBuf> = Vec::new();
            let mut patch_failed = false;

            for file_patch in &text_patch.file_patches {
                let (lookup_key, _target_key) = resolve_paths(file_patch);

                if !modified_files.contains_key(&lookup_key) {
                    modified_files.insert(lookup_key.clone(), ModifiedFile::new_non_existent());
                }
                let mf = modified_files.get_mut(&lookup_key).unwrap();

                let report = file_patch.apply(mf, PatchDirection::Forward, max_fuzz,
                                               &analyses, &fn_analysis_note_noop);
                if report.failed() {
                    if skip_failed {
                        eprintln!("WARN: hunk failed in {} (patch {})",
                            lookup_key.display(), pp.meta.source_path.display());
                        patch_failed = true;
                    } else {
                        bail!("hunk failed in '{}' while applying patch '{}'",
                            lookup_key.display(), pp.meta.source_path.display());
                    }
                }

                if file_patch.is_rename() {
                    if let Some(new_fn) = file_patch.new_filename() {
                        let new_key = new_fn.as_ref().to_path_buf();
                        let mf_entry = modified_files.get_mut(&lookup_key).unwrap();
                        let moved = mf_entry.move_out();
                        deleted_paths.push(lookup_key.clone());
                        modified_files.insert(new_key.clone(), moved);
                    }
                }
            }

            if patch_failed {
                stats.skipped_error += 1;
                continue;
            }

            let mut file_tasks = Vec::with_capacity(text_patch.file_patches.len());

            for file_patch in &text_patch.file_patches {
                let (lookup_key, _) = resolve_paths(file_patch);

                let target = if file_patch.is_rename() {
                    file_patch.new_filename()
                        .map(|p| p.as_ref().to_path_buf())
                        .unwrap_or_else(|| lookup_key.clone())
                } else {
                    lookup_key.clone()
                };

                let mf = match modified_files.get(&target) {
                    Some(mf) => mf,
                    None => continue,
                };

                if mf.deleted {
                    if !deleted_paths.contains(&target) {
                        deleted_paths.push(target);
                    }
                } else {
                    let permissions_mode = mf.permissions.as_ref().map(|p| {
                        use std::os::unix::fs::PermissionsExt;
                        p.mode()
                    });
                    let default_mode = mode_map.get(&target).copied().unwrap_or(0o100644);

                    file_tasks.push(FileTask {
                        target_path: target,
                        content: mf.content.clone(),
                        permissions_mode,
                        default_mode,
                    });
                }
            }

            patch_tasks.push(PatchTask {
                meta: pp.meta,
                file_tasks,
                deleted_paths,
            });
            stats.applied += 1;
        }

        // 2. Parallel Hashing on this chunk
        let th = std::time::Instant::now();
        let hashed_patches: Vec<HashedPatch> = patch_tasks
            .into_par_iter()
            .map_init(
                || Vec::with_capacity(256 * 1024),
                |content_buf, task| {
                    use openssl::sha::Sha1;

                    let mut blobs = Vec::with_capacity(task.file_tasks.len());
                    for file_task in task.file_tasks {
                        let mode = match file_task.permissions_mode {
                            Some(m) => m,
                            None => file_task.default_mode,
                        };
                        let mode = if mode & 0o111 != 0 { 0o100755 } else { 0o100644 };

                        content_buf.clear();
                        for line in &file_task.content {
                            content_buf.extend_from_slice(line);
                        }

                        let mut hasher = Sha1::new();
                        let mut hdr = [0u8; 26];
                        hdr[..5].copy_from_slice(b"blob ");
                        let mut n = content_buf.len();
                        let mut digits = [0u8; 20];
                        let mut nd = 0;
                        loop {
                            digits[nd] = b'0' + (n % 10) as u8;
                            nd += 1;
                            n /= 10;
                            if n == 0 { break; }
                        }
                        for i in 0..nd { hdr[5 + i] = digits[nd - 1 - i]; }
                        hdr[5 + nd] = 0;
                        hasher.update(&hdr[..6 + nd]);
                        hasher.update(content_buf);

                        let oid = git2::Oid::from_bytes(&hasher.finish()).expect("OpenSSL SHA-1 is 20 bytes");

                        // Fast thread-safe seen check with sharding
                        let shard_idx = oid.as_bytes()[0] as usize;
                        let mut seen_guard = blob_seen[shard_idx].lock().unwrap();
                        let seen = seen_guard.contains(&oid);
                        let content = if seen {
                            drop(seen_guard);
                            Vec::new() // Already seen, no content buffer clone needed!
                        } else {
                            seen_guard.insert(oid);
                            drop(seen_guard);
                            content_buf.clone() // Clone only for the first occurrence of this blob
                        };

                        blobs.push(HashedBlob {
                            path: file_task.target_path,
                            oid,
                            mode,
                            content,
                        });
                    }
                    HashedPatch {
                        meta: task.meta,
                        blobs,
                        deleted: task.deleted_paths,
                    }
                }
            )
            .collect();
        t_hash_collect += th.elapsed();

        // 3. Sequential Post-processing on this chunk
        for hp in hashed_patches {
            let mut final_blobs = Vec::with_capacity(hp.blobs.len());
            for hb in hp.blobs {
                if hb.content.is_empty() {
                    n_deduped += 1;
                } else {
                    let to = std::time::Instant::now();
                    let in_linux = git_odb.exists(hb.oid);
                    t_odb += to.elapsed();
                    if in_linux {
                        n_alternates += 1;
                    } else {
                        let tc = std::time::Instant::now();
                        shard_pool.emit_blob(hb.content)?;
                        t_shard += tc.elapsed();
                    }
                }
                final_blobs.push(BlobEntry {
                    path: hb.path,
                    oid: hb.oid,
                    mode: hb.mode,
                });
            }
            results.push(PatchResult {
                meta: hp.meta,
                blobs: final_blobs,
                deleted: hp.deleted,
            });
        }
    }

    if !quiet {
        eprintln!(
            "  apply timing: hash_collect={:.1}s  odb={:.1}s  shard_send={:.1}s  alternates={n_alternates}  deduped={n_deduped}",
            t_hash_collect.as_secs_f64(), t_odb.as_secs_f64(), t_shard.as_secs_f64()
        );
    }

    Ok((results, stats))
}

/// Apply all patches and squash into a single commit (one-shot mode).
/// Only blobs for the final state of each touched file are emitted.
pub fn apply_blobs_one_shot<'a>(
    parsed: Vec<ParsedPatch<'a>>,
    mut modified_files: HashMap<PathBuf, ModifiedFile<'a>>,
    mode_map: HashMap<PathBuf, u32>,
    git_odb: &git2::Odb,
    shard_pool: &mut BlobShardPool,
    max_fuzz: usize,
    skip_failed: bool,
    verbose: bool,
    quiet: bool,
) -> Result<(PatchResult, Stats)> {
    use crate::patch_header::PatchMeta;

    let mut stats = Stats { applied: 0, skipped_binary: 0, skipped_error: 0 };
    let analyses = AnalysisSet::default();

    let mut touched_paths: HashSet<PathBuf> = HashSet::new();
    let mut rename_sources: HashSet<PathBuf> = HashSet::new();

    // Apply all patches, tracking which paths end up changed
    for pp in parsed {
        let text_patch = match pp.text_patch {
            None => {
                eprintln!("SKIP (binary): {}", pp.meta.source_path.display());
                stats.skipped_binary += 1;
                continue;
            }
            Some(tp) => tp,
        };

        if verbose {
            println!("  applying: {}", pp.meta.source_path.display());
        }

        let mut patch_failed = false;

        for file_patch in &text_patch.file_patches {
            let (lookup_key, _target_key) = resolve_paths(file_patch);

            if !modified_files.contains_key(&lookup_key) {
                modified_files.insert(lookup_key.clone(), ModifiedFile::new_non_existent());
            }
            let mf = modified_files.get_mut(&lookup_key).unwrap();

            let report = file_patch.apply(mf, PatchDirection::Forward, max_fuzz,
                                           &analyses, &fn_analysis_note_noop);
            if report.failed() {
                if skip_failed {
                    eprintln!("WARN: hunk failed in {} (patch {})",
                        lookup_key.display(), pp.meta.source_path.display());
                    patch_failed = true;
                } else {
                    bail!("hunk failed in '{}' while applying patch '{}'",
                        lookup_key.display(), pp.meta.source_path.display());
                }
            }

            if file_patch.is_rename() {
                if let Some(new_fn) = file_patch.new_filename() {
                    let new_key = new_fn.as_ref().to_path_buf();
                    let mf_entry = modified_files.get_mut(&lookup_key).unwrap();
                    let moved = mf_entry.move_out();
                    rename_sources.insert(lookup_key.clone());
                    touched_paths.remove(&lookup_key);
                    modified_files.insert(new_key.clone(), moved);
                    touched_paths.insert(new_key);
                }
            } else {
                touched_paths.insert(lookup_key);
            }
        }

        if patch_failed {
            stats.skipped_error += 1;
        } else {
            stats.applied += 1;
        }
    }

    // Emit blobs for the final state of all touched paths
    let mut blob_seen: HashSet<git2::Oid> = HashSet::new();
    let mut content_buf: Vec<u8> = Vec::with_capacity(256 * 1024);
    let mut blobs: Vec<BlobEntry> = Vec::new();
    let mut deleted: Vec<PathBuf> = Vec::new();
    let mut n_alternates: usize = 0;
    let mut n_deduped: usize = 0;

    for path in &touched_paths {
        let mf = match modified_files.get(path) {
            Some(mf) => mf,
            None => continue,
        };

        if mf.deleted {
            deleted.push(path.clone());
            continue;
        }

        let mode = match &mf.permissions {
            Some(p) => {
                use std::os::unix::fs::PermissionsExt;
                p.mode()
            }
            None => mode_map.get(path).copied().unwrap_or(0o100644),
        };
        let mode = if mode & 0o111 != 0 { 0o100755 } else { 0o100644 };

        let oid = collect_and_hash(mf, &mut content_buf);

        if blob_seen.contains(&oid) {
            n_deduped += 1;
        } else {
            blob_seen.insert(oid);
            let in_linux = git_odb.exists(oid);
            if in_linux {
                n_alternates += 1;
            } else {
                shard_pool.emit_blob(content_buf.clone())
                    .context("emitting blob to shard")?;
            }
        }

        blobs.push(BlobEntry { path: path.clone(), oid, mode });
    }

    // Rename sources must be deleted from the base tree
    for src in rename_sources {
        if !touched_paths.contains(&src) {
            deleted.push(src);
        }
    }

    if !quiet {
        eprintln!(
            "  one-shot: {} blobs, {} deleted, alternates={n_alternates}  deduped={n_deduped}",
            blobs.len(), deleted.len()
        );
    }

    // Synthetic meta for the single squashed commit; caller sets author_timestamp
    let meta = PatchMeta {
        source_path: PathBuf::new(),
        author_name: "kexpand".to_string(),
        author_email: "kexpand@localhost".to_string(),
        author_timestamp: 0,
        author_tz: "+0000".to_string(),
        subject: format!("Apply {} SUSE kernel patches", stats.applied),
        references: String::new(),
        patch_mainline: String::new(),
        git_commit: String::new(),
        git_repo: String::new(),
        alt_commit: String::new(),
        body: String::new(),
    };

    Ok((PatchResult { meta, blobs, deleted }, stats))
}

/// Determine the lookup key (for HashMap) and target key (for output).
fn resolve_paths(file_patch: &TextFilePatch) -> (PathBuf, PathBuf) {
    match (file_patch.old_filename(), file_patch.new_filename()) {
        (Some(old), Some(new)) if file_patch.is_rename() => {
            (strip_quilt_backup(old.as_ref()), new.as_ref().to_path_buf())
        }
        (Some(_), Some(new)) => {
            let key = new.as_ref().to_path_buf();
            (key.clone(), key)
        }
        (Some(old), None) => {
            let key = strip_quilt_backup(old.as_ref());
            (key.clone(), key)
        }
        (None, Some(new)) => (new.as_ref().to_path_buf(), new.as_ref().to_path_buf()),
        (None, None) => unreachable!("file patch must have at least one filename"),
    }
}

fn strip_quilt_backup(path: &Path) -> PathBuf {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if let Some(tilde) = name.find('~') {
            if tilde > 0 {
                let parent = path.parent().unwrap_or(Path::new(""));
                return parent.join(&name[..tilde]);
            }
        }
    }
    path.to_path_buf()
}

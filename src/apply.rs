use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use libpatch::analysis::{AnalysisSet, fn_analysis_note_noop};
use libpatch::modified_file::ModifiedFile;
use libpatch::patch::unified::parser::{parse_patch, ParseError};
use libpatch::patch::{PatchDirection, TextFilePatch, TextPatch};

use crate::arena::ByteArena;
use crate::blob_shards::BlobShardPool;
use crate::git_source::GitSource;
use crate::patch_header::{parse_header, PatchMeta};

/// Blob reference during the shard phase (before mark→SHA-1 resolution).
#[derive(Clone)]
pub enum BlobMark {
    /// Already exists in the linux alternates ODB; SHA-1 is known immediately.
    Alternates(git2::Oid),
    /// Emitted to `shard[shard_idx]` with the given local mark.
    Shard(usize, u64),
}

pub struct BlobEntry {
    pub path: PathBuf,
    pub blob_mark: BlobMark,
    pub mode: u32,
}

/// One successfully-applied patch, with blob references not yet resolved to SHA-1s.
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

/// Compute git blob SHA-1 using OpenSSL's native SHA-1 (SHA-NI accelerated),
/// bypassing libgit2's sha1dc (collision-detecting, ~4× slower).
/// Git format: SHA1("blob {len}\0{content}")
fn compute_blob_oid(content: &[u8]) -> git2::Oid {
    use openssl::sha::Sha1;
    let mut hasher = Sha1::new();
    let mut hdr = [0u8; 26];
    hdr[..5].copy_from_slice(b"blob ");
    let mut n = content.len();
    let mut digits = [0u8; 20];
    let mut ndigits = 0;
    loop {
        digits[ndigits] = b'0' + (n % 10) as u8;
        ndigits += 1;
        n /= 10;
        if n == 0 { break; }
    }
    for i in 0..ndigits { hdr[5 + i] = digits[ndigits - 1 - i]; }
    hdr[5 + ndigits] = 0;
    hasher.update(&hdr[..6 + ndigits]);
    hasher.update(content);
    git2::Oid::from_bytes(&hasher.finish()).expect("OpenSSL SHA-1 is 20 bytes")
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

/// Apply all parsed patches serially, emitting blobs to `shard_pool`.
/// Returns a `PatchResult` per applied patch (for later commit emission)
/// and aggregate statistics.
pub fn apply_blobs<'a>(
    parsed: Vec<ParsedPatch<'a>>,
    mut modified_files: HashMap<PathBuf, ModifiedFile<'a>>,
    mode_map: HashMap<PathBuf, u32>,
    git_source: &GitSource,
    shard_pool: &mut BlobShardPool,
    max_fuzz: usize,
    skip_failed: bool,
    verbose: bool,
    quiet: bool,
) -> Result<(Vec<PatchResult>, Stats)> {
    let mut stats = Stats { applied: 0, skipped_binary: 0, skipped_error: 0 };
    let mut results: Vec<PatchResult> = Vec::new();
    let analyses = AnalysisSet::default();

    let mut t_write_to = std::time::Duration::ZERO;
    let mut t_sha1 = std::time::Duration::ZERO;
    let mut t_odb = std::time::Duration::ZERO;
    let mut t_shard = std::time::Duration::ZERO;
    // Cache: blob SHA-1 → BlobMark. Avoids re-emitting blobs already in shards
    // or in the linux alternates.
    let mut blob_cache: HashMap<git2::Oid, BlobMark> = HashMap::new();
    let mut n_alternates: usize = 0;
    let mut n_deduped: usize = 0;

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

        let mut blobs: Vec<BlobEntry> = Vec::new();
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
                let tw = std::time::Instant::now();
                let mut content = Vec::new();
                mf.write_to(&mut content).context("writing file content")?;
                t_write_to += tw.elapsed();

                let mode = match &mf.permissions {
                    Some(p) => {
                        use std::os::unix::fs::PermissionsExt;
                        p.mode()
                    }
                    None => mode_map.get(&target).copied().unwrap_or(0o100644),
                };
                let mode = if mode & 0o111 != 0 { 0o100755 } else { 0o100644 };

                let ts = std::time::Instant::now();
                let oid = compute_blob_oid(&content);
                t_sha1 += ts.elapsed();

                let blob_mark = if let Some(cached) = blob_cache.get(&oid) {
                    n_deduped += 1;
                    cached.clone()
                } else {
                    let to = std::time::Instant::now();
                    let in_linux = git_source.blob_exists(oid);
                    t_odb += to.elapsed();
                    if in_linux {
                        n_alternates += 1;
                        let bm = BlobMark::Alternates(oid);
                        blob_cache.insert(oid, bm.clone());
                        bm
                    } else {
                        let tc = std::time::Instant::now();
                        let (shard_idx, mark) = shard_pool.emit_blob(content)
                            .context("emitting blob to shard")?;
                        t_shard += tc.elapsed();
                        let bm = BlobMark::Shard(shard_idx, mark);
                        blob_cache.insert(oid, bm.clone());
                        bm
                    }
                };

                blobs.push(BlobEntry { path: target, blob_mark, mode });
            }
        }

        results.push(PatchResult { meta: pp.meta, blobs, deleted: deleted_paths });
        stats.applied += 1;
    }

    if !quiet {
        eprintln!(
            "  apply timing: write_to={:.1}s  sha1={:.1}s  odb={:.1}s  shard_send={:.1}s  alternates={n_alternates}  deduped={n_deduped}",
            t_write_to.as_secs_f64(), t_sha1.as_secs_f64(),
            t_odb.as_secs_f64(), t_shard.as_secs_f64()
        );
    }

    Ok((results, stats))
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

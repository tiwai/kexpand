use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use libpatch::analysis::{AnalysisSet, fn_analysis_note_noop};
use libpatch::modified_file::ModifiedFile;
use libpatch::patch::unified::parser::{parse_patch, ParseError};
use libpatch::patch::{PatchDirection, TextFilePatch, TextPatch};

use crate::arena::ByteArena;
use crate::fast_import::{FastImport, Parent};
use crate::git_source::GitSource;
use crate::patch_header::{parse_header, PatchMeta};

pub struct ParsedPatch<'a> {
    pub meta: PatchMeta,
    pub text_patch: Option<TextPatch<'a>>,  // None = binary or parse error we skip
}

pub struct Stats {
    pub applied: usize,
    pub skipped_binary: usize,
    pub skipped_error: usize,
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

/// Apply all parsed patches serially, streaming commits to `fi`.
pub fn apply_all<'a, W: Write>(
    parsed: Vec<ParsedPatch<'a>>,
    git_source: &GitSource,
    arena: &'a ByteArena<'a>,
    fi: &mut FastImport<W>,
    branch: &str,
    max_fuzz: usize,
    skip_failed: bool,
    committer_timestamp: i64,
    verbose: bool,
) -> Result<Stats> {
    let mut stats = Stats { applied: 0, skipped_binary: 0, skipped_error: 0 };
    let mut modified_files: HashMap<PathBuf, ModifiedFile<'a>> = HashMap::new();
    let mut parent: Option<u64> = None;
    let base_sha1 = git_source.base_commit_sha1().to_string();
    let analyses = AnalysisSet::default();

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

        let mut modified: Vec<(PathBuf, u64, u32)> = Vec::new();
        let mut deleted_paths: Vec<PathBuf> = Vec::new();
        let mut patch_failed = false;

        for file_patch in &text_patch.file_patches {
            let (lookup_key, _target_key) = resolve_paths(file_patch);

            // Get or load the file
            if !modified_files.contains_key(&lookup_key) {
                let mf = load_or_create(git_source, arena, &lookup_key)?;
                modified_files.insert(lookup_key.clone(), mf);
            }
            let mf = modified_files.get_mut(&lookup_key).unwrap();

            // Apply the file patch
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

            // Handle renames
            if file_patch.is_rename() {
                if let Some(new_fn) = file_patch.new_filename() {
                    let new_key = new_fn.as_ref().to_path_buf();
                    // Move content from old key to new key
                    let mf_entry = modified_files.get_mut(&lookup_key).unwrap();
                    let moved = mf_entry.move_out();
                    // old entry is now 'deleted'
                    deleted_paths.push(lookup_key.clone());
                    modified_files.insert(new_key.clone(), moved);
                }
            }
        }

        if patch_failed {
            stats.skipped_error += 1;
            continue;
        }

        // Collect changed/deleted files from this patch's file_patches
        for file_patch in &text_patch.file_patches {
            let (lookup_key, _) = resolve_paths(file_patch);

            // After rename, the new path is what matters; old path is in deleted_paths
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
                // Collect file content into bytes for blob emission
                let mut content = Vec::new();
                mf.write_to(&mut content).context("writing file content")?;

                // Determine file mode
                let mode = match &mf.permissions {
                    Some(p) => {
                        use std::os::unix::fs::PermissionsExt;
                        p.mode()
                    }
                    None => git_source.file_mode(&target),
                };
                // Normalize to 100644 or 100755
                let mode = if mode & 0o111 != 0 { 0o100755 } else { 0o100644 };

                let blob_mark = fi.emit_blob(&content)
                    .context("emitting blob")?;
                modified.push((target, blob_mark, mode));
            }
        }

        // Emit commit even if no files changed (some patches are metadata-only)
        let parent_ref = match parent {
            None => Parent::BaseSha1(base_sha1.clone()),
            Some(m) => Parent::Mark(m),
        };

        let deleted_refs: Vec<&Path> = deleted_paths.iter().map(PathBuf::as_path).collect();
        let modified_refs: Vec<(&Path, u64, u32)> = modified.iter()
            .map(|(p, m, mode)| (p.as_path(), *m, *mode))
            .collect();

        let commit_mark = fi.emit_commit(
            &pp.meta,
            branch,
            parent_ref,
            committer_timestamp,
            &modified_refs,
            &deleted_refs,
        ).context("emitting commit")?;

        parent = Some(commit_mark);
        stats.applied += 1;
    }

    fi.flush().context("flushing fast-import")?;
    Ok(stats)
}

/// Determine the lookup key (for HashMap) and target key (for output).
fn resolve_paths(file_patch: &TextFilePatch) -> (PathBuf, PathBuf) {
    match (file_patch.old_filename(), file_patch.new_filename()) {
        (Some(old), Some(new)) if file_patch.is_rename() => {
            // Rename: look up the old path (stripping any quilt backup suffix),
            // output goes to the new path.
            (strip_quilt_backup(old.as_ref()), new.as_ref().to_path_buf())
        }
        (Some(_), Some(new)) => {
            // Regular modify: new_name is always the canonical path.
            // old_name may carry a backup suffix (~patchname, .orig, .alt …)
            // added by quilt or patch(1); new_name never has one.
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

/// Strip the quilt backup suffix (~patchname) from the final path component.
/// Used for rename patches where old_name may be "oldfile.c~patchname".
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

fn load_or_create<'a>(
    git_source: &GitSource,
    arena: &'a ByteArena<'a>,
    path: &Path,
) -> Result<ModifiedFile<'a>> {
    match git_source.load_file(path)? {
        Some(bytes) => {
            let slice: &'a [u8] = arena.alloc(bytes);
            Ok(ModifiedFile::new(slice, true, None))
        }
        None => Ok(ModifiedFile::new_non_existent()),
    }
}

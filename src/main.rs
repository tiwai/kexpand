mod arena;
mod apply;
mod blob_shards;
mod fast_import;
mod git_source;
mod kernel_source;
mod patch_header;
mod series;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::Parser;
use libpatch::modified_file::ModifiedFile;

use crate::apply::{apply_blobs, collect_patch_paths, parse_patches, BlobMark, PatchResult};
use crate::arena::ByteArena;
use crate::blob_shards::BlobShardPool;
use crate::fast_import::{BlobRef, FastImport, Parent};
use crate::git_source::GitSource;
use crate::kernel_source::KernelSource;

#[derive(Parser, Debug)]
#[command(about = "Expand SUSE kernel-source patches into a git repository.\n\
    Run from the root of a kernel-source.git working tree.")]
struct Cli {
    /// Output branch name in the destination repo
    #[arg(short = 'b', long, default_value = "master")]
    branch: String,

    /// Path to upstream linus git repo (read-only) [$LINUX_GIT]
    #[arg(short = 'l', long, env = "LINUX_GIT")]
    linus: Option<PathBuf>,

    /// Output git repository to create
    output: PathBuf,

    /// Number of threads for parallel processing (0 = auto)
    #[arg(short = 'j', long, default_value = "0")]
    jobs: usize,

    /// Maximum fuzz for hunk matching
    #[arg(long, default_value = "2")]
    max_fuzz: usize,

    /// Skip patches that fail to apply instead of aborting
    #[arg(long)]
    skip_failed: bool,

    /// Print each patch as it's applied
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Parse and apply but don't write to git (testing only)
    #[arg(long)]
    dry_run: bool,

    /// Skip the final `git repack` step (leaves loose/uncompressed packs)
    #[arg(long)]
    no_repack: bool,

    /// Threads for `git repack` (0 = auto)
    #[arg(long, default_value = "0")]
    repack_threads: usize,

    /// Number of parallel blob-shard fast-import processes (0 = auto)
    #[arg(long, default_value = "0")]
    blob_shards: usize,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let linus = cli.linus
        .context("--linus / $LINUX_GIT is required")?;

    // Set up thread pool
    if cli.jobs > 0 {
        rayon::ThreadPoolBuilder::new()
            .num_threads(cli.jobs)
            .build_global()
            .context("building rayon thread pool")?;
    }

    // Open kernel-source working tree at the current directory
    let ks = KernelSource::open(Path::new("."))
        .context("opening kernel-source (run from its working-tree root)")?;

    // Detect base tag from rpm/config.sh
    let base_tag = ks.base_tag()?;
    println!("Base tag: {}", base_tag);

    // Parse series.conf
    let series = ks.series()?;
    println!("Patches to apply: {}", series.len());

    // Set up output repo
    if !cli.dry_run {
        init_output_repo(&cli.output, &linus)?;
    }

    let arena = ByteArena::new();

    let t0 = Instant::now();
    println!("Reading patches...");
    let patch_entries = ks.read_patches(&series)?;
    println!("Read {} patches in {:.1}s", patch_entries.len(), t0.elapsed().as_secs_f64());

    // Parse patches in parallel
    let t1 = Instant::now();
    println!("Parsing patches...");
    let parsed = parse_patches(patch_entries, &arena)?;
    println!("Parsed {} patches in {:.1}s", parsed.len(), t1.elapsed().as_secs_f64());

    // Open the linus git source
    let git_source = GitSource::new(&linus, &base_tag)?;
    println!("Base commit: {}", git_source.base_commit_sha1());

    // Pre-warm: load all base-tree files touched by patches in parallel
    let t_pw = Instant::now();
    println!("Pre-loading source files...");
    let touched_paths = collect_patch_paths(&parsed);
    let prewarmed = git_source.prewarm_files(&touched_paths)
        .context("pre-warming file cache")?;
    let mut pre_files: HashMap<PathBuf, ModifiedFile> = HashMap::with_capacity(prewarmed.len());
    let mut mode_map: HashMap<PathBuf, u32> = HashMap::with_capacity(prewarmed.len());
    for (path, content, mode) in prewarmed {
        let mf = match content {
            Some(bytes) => ModifiedFile::new(arena.alloc(bytes), true, None),
            None => ModifiedFile::new_non_existent(),
        };
        pre_files.insert(path.clone(), mf);
        mode_map.insert(path, mode);
    }
    println!("Pre-loaded {} files in {:.1}s", pre_files.len(), t_pw.elapsed().as_secs_f64());

    let committer_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    if cli.dry_run {
        println!("--dry-run: skipping git output");
        return Ok(());
    }

    // Resolve shard count
    let n_shards = if cli.blob_shards == 0 {
        std::thread::available_parallelism().map_or(8, |n| n.get().min(16))
    } else {
        cli.blob_shards
    };
    println!("Blob shards: {n_shards}");

    // Phase 1: apply patches, push blobs to shard writer threads
    let mut shard_pool = BlobShardPool::new(&cli.output, n_shards)?;
    let t_apply = Instant::now();
    println!("Applying patches...");
    let (patch_results, stats) = apply_blobs(
        parsed,
        pre_files,
        mode_map,
        &git_source,
        &mut shard_pool,
        cli.max_fuzz,
        cli.skip_failed,
        cli.verbose,
    )?;
    println!("Applied {} patches in {:.1}s", stats.applied, t_apply.elapsed().as_secs_f64());

    // Phase 2: drain all shard fast-imports in parallel, collect mark→SHA-1 maps
    let t_drain = Instant::now();
    println!("Draining blob shards...");
    let mark_maps = shard_pool.finish_and_read_marks()?;
    println!("Blob shards drained in {:.1}s", t_drain.elapsed().as_secs_f64());

    // Phase 3: emit commits (no blob data, just commit metadata + SHA-1 blob refs)
    let t_commits = Instant::now();
    println!("Emitting commits...");
    let mut commit_child = Command::new("git")
        .args(["-c", "pack.compression=0", "fast-import", "--done", "--force"])
        .current_dir(&cli.output)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawning commit fast-import")?;
    let commit_stdin = commit_child.stdin.take().unwrap();
    let mut fi = FastImport::new(commit_stdin);
    emit_commits(
        &mut fi,
        &patch_results,
        &mark_maps,
        git_source.base_commit_sha1(),
        &cli.branch,
        committer_timestamp,
    )?;
    fi.done().context("sending done to commit fast-import")?;
    drop(fi);
    let status = commit_child.wait().context("waiting for commit fast-import")?;
    if !status.success() {
        bail!("commit fast-import failed with {status}");
    }
    println!("Commits emitted in {:.1}s", t_commits.elapsed().as_secs_f64());

    println!(
        "{} commits, {} binary skipped, {} failed skipped",
        stats.applied, stats.skipped_binary, stats.skipped_error
    );

    if !cli.no_repack {
        let t_repack = Instant::now();
        println!("Repacking...");
        run_repack(&cli.output, cli.repack_threads)?;
        println!("Repack done in {:.1}s", t_repack.elapsed().as_secs_f64());
    }

    Ok(())
}

fn emit_commits<W: std::io::Write>(
    fi: &mut FastImport<W>,
    results: &[PatchResult],
    mark_maps: &[HashMap<u64, git2::Oid>],
    base_commit_sha1: &str,
    branch: &str,
    committer_timestamp: i64,
) -> Result<()> {
    let mut parent_mark: Option<u64> = None;

    for result in results {
        let modified: Vec<(&Path, BlobRef, u32)> = result.blobs.iter()
            .map(|e| -> Result<(&Path, BlobRef, u32)> {
                let br = match &e.blob_mark {
                    BlobMark::Alternates(oid) => BlobRef::Sha1(*oid),
                    BlobMark::Shard(idx, mark) => {
                        let oid = mark_maps[*idx].get(mark)
                            .with_context(|| format!("shard {idx} mark :{mark} missing from marks file"))?;
                        BlobRef::Sha1(*oid)
                    }
                };
                Ok((e.path.as_path(), br, e.mode))
            })
            .collect::<Result<_>>()?;

        let deleted: Vec<&Path> = result.deleted.iter().map(|p| p.as_path()).collect();

        let parent = match parent_mark {
            None => Parent::BaseSha1(base_commit_sha1.to_string()),
            Some(m) => Parent::Mark(m),
        };

        let cm = fi.emit_commit(&result.meta, branch, parent, committer_timestamp, &modified, &deleted)?;
        parent_mark = Some(cm);
    }

    Ok(())
}

/// Repack the fast-import pack using parallel delta-compression, WITHOUT
/// scanning the alternates (linus) graph. Strategy:
///
/// 1. List all objects in the existing pack via `git show-index`.
/// 2. Pipe those SHAs into `git pack-objects --delta-base-offset` which finds
///    deltas among local objects only (no alternates walk = no ~80s linus scan).
/// 3. Atomically swap the new pack in and delete the old one.
fn run_repack(output: &std::path::Path, threads: usize) -> Result<()> {
    use std::io::{BufRead, BufReader, BufWriter, Write};

    let threads_str = threads.to_string();
    let pack_dir = output.join(".git/objects/pack");

    // Find the single pack that fast-import wrote.
    let idx_files: Vec<std::path::PathBuf> = std::fs::read_dir(&pack_dir)
        .context("reading pack dir")?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |e| e == "idx"))
        .collect();

    if idx_files.is_empty() {
        return Ok(());
    }

    let pack_base = std::path::Path::new(".git/objects/pack/pack");
    let mut po = Command::new("git")
        .args([
            "pack-objects",
            "--delta-base-offset",
            "--compression=9",
            &format!("--threads={threads_str}"),
        ])
        .arg(&pack_base)
        .current_dir(output)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawning git pack-objects")?;

    let po_stdin = po.stdin.take().unwrap();
    let po_stdout = po.stdout.take().unwrap();

    // Write all object SHAs from every existing idx to pack-objects stdin.
    let idx_files_clone = idx_files.clone();
    let output_owned = output.to_path_buf();
    let write_handle = std::thread::spawn(move || -> Result<()> {
        let mut w = BufWriter::new(po_stdin);
        for idx in &idx_files_clone {
            let mut si = Command::new("git")
                .args(["show-index"])
                .current_dir(&output_owned)
                .stdin(std::fs::File::open(idx).context("opening idx")?)
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .context("spawning git show-index")?;
            let si_out = si.stdout.take().unwrap();
            let reader = BufReader::new(si_out);
            for line in reader.lines() {
                let line = line.context("reading show-index output")?;
                // format: "<offset> <sha1> <crc>" — SHA is field 1
                if let Some(sha) = line.split_whitespace().nth(1) {
                    w.write_all(sha.as_bytes())?;
                    w.write_all(b"\n")?;
                }
            }
            si.wait().context("waiting for git show-index")?;
        }
        Ok(())
    });

    let mut new_sha = String::new();
    BufReader::new(po_stdout).read_line(&mut new_sha).context("reading pack-objects output")?;
    let new_sha = new_sha.trim().to_string();

    write_handle.join().unwrap().context("writing pack-objects stdin")?;
    let status = po.wait().context("waiting for git pack-objects")?;
    if !status.success() {
        bail!("git pack-objects failed");
    }

    // Delete the old pack files.
    for idx in &idx_files {
        let pack = idx.with_extension("pack");
        std::fs::remove_file(&pack)
            .with_context(|| format!("removing old pack {}", pack.display()))?;
        std::fs::remove_file(idx)
            .with_context(|| format!("removing old idx {}", idx.display()))?;
    }

    let new_pack = pack_dir.join(format!("pack-{new_sha}.pack"));
    let new_idx  = pack_dir.join(format!("pack-{new_sha}.idx"));
    if !new_pack.exists() || !new_idx.exists() {
        bail!("pack-objects wrote pack-{new_sha} but files not found");
    }

    Ok(())
}

/// Initialize the output git repo with alternates pointing to linus objects.
fn init_output_repo(output: &std::path::Path, linus: &std::path::Path) -> Result<()> {
    if output.exists() {
        println!("Output repo exists, using it");
        return Ok(());
    }

    let status = Command::new("git")
        .args(["init", output.to_str().context("output path not UTF-8")?])
        .status()
        .context("git init")?;
    if !status.success() {
        bail!("git init failed");
    }

    // Write alternates to share objects with linus repo
    let info_dir = output.join(".git/objects/info");
    std::fs::create_dir_all(&info_dir).context("creating objects/info")?;
    let linus_objects = linus.canonicalize()
        .with_context(|| format!("canonicalizing {}", linus.display()))?
        .join(".git/objects");
    std::fs::write(
        info_dir.join("alternates"),
        format!("{}\n", linus_objects.display()),
    ).context("writing alternates")?;

    println!("Initialized output repo at {}", output.display());
    Ok(())
}

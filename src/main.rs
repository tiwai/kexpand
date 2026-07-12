mod arena;
mod apply;
mod blob_shards;
mod commit_pack;
mod git_source;
mod kernel_source;
mod patch_header;
mod series;
mod tree_state;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::Parser;
use libpatch::modified_file::ModifiedFile;

use crate::apply::{apply_blobs, apply_blobs_one_shot, collect_patch_paths, parse_patches, PatchResult};
use crate::arena::ByteArena;
use crate::blob_shards::BlobShardPool;
use crate::commit_pack::CommitPackWriter;
use crate::git_source::GitSource;
use crate::kernel_source::KernelSource;
use crate::tree_state::{encode_commit, TreeState};

#[derive(Parser, Debug)]
#[command(about = "Expand SUSE kernel-source patches into a git repository.\n\
    Run from the root of a kernel-source.git working tree.")]
struct Cli {
    /// Output branch name in the destination repo
    #[arg(short = 'b', long, default_value = "master")]
    branch: String,

    /// Path to upstream linux git repo (read-only) [$LINUX_GIT]
    #[arg(short = 'l', long, env = "LINUX_GIT")]
    linux: Option<PathBuf>,

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

    /// Suppress progress output
    #[arg(short = 'q', long)]
    quiet: bool,

    /// Skip the final `git repack` step (leaves loose/uncompressed packs)
    #[arg(short = 'R', long)]
    no_repack: bool,

    /// Threads for `git repack` (0 = auto)
    #[arg(long, default_value = "0")]
    repack_threads: usize,

    /// Number of parallel blob-shard fast-import processes (0 = auto)
    #[arg(long, default_value = "0")]
    blob_shards: usize,

    /// Squash all patches into a single commit instead of one commit per patch
    #[arg(short = '1', long)]
    one_shot: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let linux = cli.linux
        .context("--linux / $LINUX_GIT is required")?;

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
    if !cli.quiet { println!("Base tag: {}", base_tag); }

    // Parse series.conf
    let series = ks.series()?;
    if !cli.quiet { println!("Patches to apply: {}", series.len()); }

    // Set up output repo
    if !cli.dry_run {
        init_output_repo(&cli.output, &linux, cli.quiet)?;
    }

    let arena = ByteArena::new();

    let t0 = Instant::now();
    if !cli.quiet { println!("Reading patches..."); }
    let patch_entries = ks.read_patches(&series)?;
    if !cli.quiet { println!("Read {} patches in {:.1}s", patch_entries.len(), t0.elapsed().as_secs_f64()); }

    // Parse patches in parallel
    let t1 = Instant::now();
    if !cli.quiet { println!("Parsing patches..."); }
    let parsed = parse_patches(patch_entries, &arena)?;
    if !cli.quiet { println!("Parsed {} patches in {:.1}s", parsed.len(), t1.elapsed().as_secs_f64()); }

    // Open the linux git source
    let git_source = GitSource::new(&linux, &base_tag)?;
    if !cli.quiet { println!("Base commit: {}", git_source.base_commit_sha1()); }

    // Pre-warm: load all base-tree files touched by patches in parallel
    let t_pw = Instant::now();
    if !cli.quiet { println!("Pre-loading source files..."); }
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
    if !cli.quiet { println!("Pre-loaded {} files in {:.1}s", pre_files.len(), t_pw.elapsed().as_secs_f64()); }

    let committer_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    if cli.dry_run {
        if !cli.quiet { println!("--dry-run: skipping git output"); }
        return Ok(());
    }

    // Resolve shard count
    let n_shards = if cli.blob_shards == 0 {
        std::thread::available_parallelism().map_or(8, |n| n.get().min(16))
    } else {
        cli.blob_shards
    };
    if !cli.quiet { println!("Blob shards: {n_shards}"); }

    // Phase 1: apply patches, push blobs to shard writer threads
    let mut shard_pool = BlobShardPool::new(&cli.output, n_shards)?;
    let t_apply = Instant::now();
    if !cli.quiet { println!("Applying patches..."); }
    let (patch_results, stats) = if cli.one_shot {
        let (mut result, stats) = apply_blobs_one_shot(
            parsed,
            pre_files,
            mode_map,
            &git_source,
            &mut shard_pool,
            cli.max_fuzz,
            cli.skip_failed,
            cli.verbose,
            cli.quiet,
        )?;
        result.meta.author_timestamp = committer_timestamp;
        (vec![result], stats)
    } else {
        apply_blobs(
            parsed,
            pre_files,
            mode_map,
            &git_source,
            &mut shard_pool,
            cli.max_fuzz,
            cli.skip_failed,
            cli.verbose,
            cli.quiet,
        )?
    };
    if !cli.quiet { println!("Applied {} patches in {:.1}s", stats.applied, t_apply.elapsed().as_secs_f64()); }

    // Phase 2: drain all blob shard fast-imports (blobs must land in the ODB
    //           before the tree objects reference their SHA-1s).
    let t_drain = Instant::now();
    if !cli.quiet { println!("Draining blob shards..."); }
    shard_pool.finish().context("draining blob shards")?;
    if !cli.quiet { println!("Blob shards drained in {:.1}s", t_drain.elapsed().as_secs_f64()); }

    // Phase 3: build trees and commits directly in Rust — no external process.
    let t_commits = Instant::now();
    if !cli.quiet { println!("Building commits..."); }
    let base_sha1 = git_source.base_commit_sha1().to_string();
    let repo = git2::Repository::open(&cli.output).context("opening output repo")?;
    build_commits_with_trees(
        &patch_results, &base_sha1, &cli.branch, committer_timestamp, &repo, &cli.output, cli.quiet,
    )?;
    if !cli.quiet { println!("Commits built in {:.1}s", t_commits.elapsed().as_secs_f64()); }

    if !cli.quiet {
        println!(
            "{} commits, {} binary skipped, {} failed skipped",
            stats.applied, stats.skipped_binary, stats.skipped_error
        );
    }

    if !cli.no_repack {
        let t_repack = Instant::now();
        if !cli.quiet { println!("Repacking..."); }
        run_repack(&cli.output, cli.repack_threads)?;
        if !cli.quiet { println!("Repack done in {:.1}s", t_repack.elapsed().as_secs_f64()); }
    }

    Ok(())
}

/// Build all commits by writing tree and commit objects directly into a new pack
/// file, bypassing any external git process.  Updates the branch ref on success.
fn build_commits_with_trees(
    results: &[PatchResult],
    base_commit_sha1: &str,
    branch: &str,
    committer_timestamp: i64,
    repo: &git2::Repository,
    output: &Path,
    quiet: bool,
) -> Result<()> {
    let t0 = Instant::now();
    let mut tree_state = TreeState::new(repo, base_commit_sha1, results)
        .context("initializing tree state from base commit")?;
    if !quiet {
        eprintln!("  tree state init: {:.2}s ({} dirs)", t0.elapsed().as_secs_f64(), results.len());
    }

    let pack_dir = output.join(".git/objects/pack");
    let mut pack = CommitPackWriter::create(&pack_dir)?;

    let base_oid = git2::Oid::from_str(base_commit_sha1)?;
    let mut parent_oid: Option<git2::Oid> = Some(base_oid);

    for result in results {
        for blob in &result.blobs {
            tree_state.update(&blob.path, blob.oid, blob.mode)
                .with_context(|| format!("update blob '{}'", blob.path.display()))?;
        }
        for deleted in &result.deleted {
            tree_state.delete(deleted)
                .with_context(|| format!("delete '{}'", deleted.display()))?;
        }

        let tree_oid = tree_state.flush(&mut pack)
            .with_context(|| format!("flush tree for '{}'", result.meta.source_path.display()))?;

        let commit_bytes = encode_commit(tree_oid, parent_oid, &result.meta, committer_timestamp);
        let commit_oid = pack.write_object(1 /* commit */, &commit_bytes)?;
        parent_oid = Some(commit_oid);
    }

    pack.finish().context("finalizing commit pack")?;

    // Point the branch ref at the last commit
    let last_oid = parent_oid.ok_or_else(|| anyhow::anyhow!("no commits written"))?;
    repo.reference(
        &format!("refs/heads/{branch}"),
        last_oid,
        true,
        "kexpand",
    ).context("updating branch ref")?;

    Ok(())
}

/// Repack the fast-import pack using parallel delta-compression, WITHOUT
/// scanning the alternates (linux) graph. Strategy:
///
/// 1. List all objects in the existing pack via `git show-index`.
/// 2. Pipe those SHAs into `git pack-objects --delta-base-offset` which finds
///    deltas among local objects only (no alternates walk = no ~80s linux scan).
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

/// Initialize the output git repo with alternates pointing to linux objects.
fn init_output_repo(output: &std::path::Path, linux: &std::path::Path, quiet: bool) -> Result<()> {
    if output.exists() {
        if !quiet { println!("Output repo exists, using it"); }
        return Ok(());
    }

    let status = Command::new("git")
        .args(["init", output.to_str().context("output path not UTF-8")?])
        .status()
        .context("git init")?;
    if !status.success() {
        bail!("git init failed");
    }

    // Write alternates to share objects with linux repo
    let info_dir = output.join(".git/objects/info");
    std::fs::create_dir_all(&info_dir).context("creating objects/info")?;
    let linux_objects = linux.canonicalize()
        .with_context(|| format!("canonicalizing {}", linux.display()))?
        .join(".git/objects");
    std::fs::write(
        info_dir.join("alternates"),
        format!("{}\n", linux_objects.display()),
    ).context("writing alternates")?;

    if !quiet { println!("Initialized output repo at {}", output.display()); }
    Ok(())
}

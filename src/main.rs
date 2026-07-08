mod arena;
mod apply;
mod fast_import;
mod git_source;
mod kernel_source;
mod patch_header;
mod series;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::Parser;
use libpatch::modified_file::ModifiedFile;

use crate::apply::{apply_all, collect_patch_paths, parse_patches};
use crate::arena::ByteArena;
use crate::fast_import::FastImport;
use crate::git_source::GitSource;
use crate::kernel_source::KernelSource;

#[derive(Parser, Debug)]
#[command(about = "Expand SUSE kernel-source patches into a git repository")]
struct Cli {
    /// Path to kernel-source.git (bare clone or working tree)
    #[arg(short = 's', long, default_value = "/home/tiwai/kernel/suse/kernel-source")]
    source: PathBuf,

    /// Branch name in kernel-source.git to read from (also used as output branch name)
    #[arg(short = 'b', long, default_value = "master")]
    branch: String,

    /// Path to upstream linus git repo (read-only)
    #[arg(short = 'l', long, default_value = "/home/tiwai/git/linus")]
    linus: PathBuf,

    /// Output git repository to create
    #[arg(short = 'o', long)]
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Set up thread pool
    if cli.jobs > 0 {
        rayon::ThreadPoolBuilder::new()
            .num_threads(cli.jobs)
            .build_global()
            .context("building rayon thread pool")?;
    }

    // Open kernel-source at the specified branch
    let ks = KernelSource::open(&cli.source, &cli.branch)
        .with_context(|| format!("opening kernel-source branch '{}'", cli.branch))?;

    // Detect base tag from rpm/config.sh on the branch
    let base_tag = ks.base_tag()?;
    println!("Branch: {}  Base tag: {}", cli.branch, base_tag);

    // Parse series.conf from the branch
    let series = ks.series()?;
    println!("Patches to apply: {}", series.len());

    // Set up output repo
    if !cli.dry_run {
        init_output_repo(&cli.output, &cli.linus)?;
    }

    let arena = ByteArena::new();

    // Read all patch blobs from the branch via git cat-file --batch
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
    let git_source = GitSource::new(&cli.linus, &base_tag)?;
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

    // Spawn git fast-import with compression disabled; a parallel repack at the
    // end will delta-compress and zlib-compress everything on multiple cores,
    // which is much faster than fast-import's single-threaded zlib path.
    let mut child = Command::new("git")
        .args(["-c", "pack.compression=0", "fast-import", "--done"])
        .current_dir(&cli.output)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawning git fast-import")?;

    let stdin = child.stdin.take().unwrap();
    let mut fi = FastImport::new(stdin);

    // Apply all patches
    let t_apply = Instant::now();
    println!("Applying patches...");
    let stats = apply_all(
        parsed,
        git_source.base_commit_sha1(),
        pre_files,
        mode_map,
        &mut fi,
        &cli.branch,
        cli.max_fuzz,
        cli.skip_failed,
        committer_timestamp,
        cli.verbose,
    )?;
    println!("Applied {} patches in {:.1}s", stats.applied, t_apply.elapsed().as_secs_f64());

    // Signal git fast-import we're done (required by --done flag)
    fi.done().context("sending done to git fast-import")?;
    drop(fi);

    // Wait for git fast-import to finish
    let t_drain = Instant::now();
    let status = child.wait().context("waiting for git fast-import")?;
    if !status.success() {
        bail!("git fast-import failed with exit code {:?}", status.code());
    }
    println!("git fast-import drained in {:.1}s", t_drain.elapsed().as_secs_f64());

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

    // Use pack-objects to enumerate all objects from the existing idx files
    // and write a new, compressed+delta pack.  This never touches alternates.
    // Path is relative to current_dir(output) — must NOT use pack_dir (which
    // may itself be a relative path, causing double-prefixing).
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
    // Run in a thread to avoid deadlock if pack-objects buffers stdout.
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

    // Read the new pack SHA that pack-objects prints to stdout.
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

    // Update the pack-file index so git finds the new pack.
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

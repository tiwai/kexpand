mod arena;
mod apply;
mod fast_import;
mod git_source;
mod kernel_source;
mod patch_header;
mod series;

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::Parser;

use crate::apply::{apply_all, parse_patches};
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

    /// Number of threads for parallel parsing (0 = auto)
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

    // Read all patch blobs from the branch (serial git2 access)
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

    let committer_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    if cli.dry_run {
        println!("--dry-run: skipping git output");
        return Ok(());
    }

    // Spawn git fast-import
    let mut child = Command::new("git")
        .args(["fast-import", "--done"])
        .current_dir(&cli.output)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawning git fast-import")?;

    let stdin = child.stdin.take().unwrap();
    let mut fi = FastImport::new(stdin);

    // Apply all patches
    let t2 = Instant::now();
    println!("Applying patches...");
    let stats = apply_all(
        parsed,
        &git_source,
        &arena,
        &mut fi,
        &cli.branch,
        cli.max_fuzz,
        cli.skip_failed,
        committer_timestamp,
        cli.verbose,
    )?;

    // Signal git fast-import we're done (required by --done flag)
    fi.done().context("sending done to git fast-import")?;
    drop(fi);

    // Wait for git fast-import to finish
    let status = child.wait().context("waiting for git fast-import")?;
    if !status.success() {
        bail!("git fast-import failed with exit code {:?}", status.code());
    }

    let elapsed = t2.elapsed().as_secs_f64();
    println!(
        "Done in {:.1}s: {} commits, {} binary skipped, {} failed skipped",
        elapsed, stats.applied, stats.skipped_binary, stats.skipped_error
    );

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

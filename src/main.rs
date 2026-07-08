mod arena;
mod apply;
mod fast_import;
mod git_source;
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
use crate::series::parse_series;

#[derive(Parser, Debug)]
#[command(about = "Expand SUSE kernel-source patches into a git repository")]
struct Cli {
    /// Path to kernel-source.git checkout
    #[arg(short = 's', long, default_value = "/home/tiwai/kernel/suse/kernel-source")]
    source: PathBuf,

    /// Path to upstream linus git repo (read-only)
    #[arg(short = 'l', long, default_value = "/home/tiwai/git/linus")]
    linus: PathBuf,

    /// Output git repository to create
    #[arg(short = 'o', long)]
    output: PathBuf,

    /// Branch name to create in the output repo
    #[arg(short = 'b', long, default_value = "expand")]
    branch: String,

    /// Number of threads for parallel parsing (0 = auto)
    #[arg(short = 'j', long, default_value = "0")]
    jobs: usize,

    /// Base kernel tag (auto-detected from rpm/config.sh if not set)
    #[arg(long)]
    base_tag: Option<String>,

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

    // Detect base tag from rpm/config.sh
    let base_tag = match cli.base_tag {
        Some(t) => t,
        None => detect_base_tag(&cli.source)?,
    };
    println!("Base tag: {}", base_tag);

    // Parse series.conf
    let series_conf = cli.source.join("series.conf");
    let series = parse_series(&series_conf)?;
    println!("Patches to apply: {}", series.len());

    // Set up output repo
    if !cli.dry_run {
        init_output_repo(&cli.output, &cli.linus)?;
    }

    // Use an arena for the entire run
    let arena = ByteArena::new();

    // Parse patches in parallel
    let t0 = Instant::now();
    println!("Parsing patches...");
    let parsed = parse_patches(&series, &cli.source, &arena)?;
    println!("Parsed {} patches in {:.1}s", parsed.len(), t0.elapsed().as_secs_f64());

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
        .stderr(Stdio::inherit())  // let git fast-import print errors directly
        .spawn()
        .context("spawning git fast-import")?;

    let stdin = child.stdin.take().unwrap();
    let mut fi = FastImport::new(stdin);

    // Apply all patches
    let t1 = Instant::now();
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

    let elapsed = t1.elapsed().as_secs_f64();
    println!(
        "Done in {:.1}s: {} commits, {} binary skipped, {} failed skipped",
        elapsed, stats.applied, stats.skipped_binary, stats.skipped_error
    );

    Ok(())
}

/// Read SRCVERSION from rpm/config.sh and return a git tag like "v6.4"
fn detect_base_tag(source: &std::path::Path) -> Result<String> {
    let config_sh = source.join("rpm/config.sh");
    let content = std::fs::read_to_string(&config_sh)
        .with_context(|| format!("reading {}", config_sh.display()))?;
    for line in content.lines() {
        if let Some(val) = line.strip_prefix("SRCVERSION=") {
            return Ok(format!("v{}", val.trim()));
        }
    }
    bail!("SRCVERSION not found in {}", config_sh.display())
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

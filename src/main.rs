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
use std::fs::File;
use std::io::{BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
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

    // Phase 2+3: drain blob shards in background while building commits in main thread.
    // The commit pack writes to a separate file from the blob shard packs, so there is
    // no I/O conflict.  Drain finishes in ~2s; commit building takes ~4.5s — the drain
    // time is fully hidden.  The branch ref is updated only after both tasks are done.
    let base_sha1 = git_source.base_commit_sha1().to_string();

    // Initialize tree state BEFORE starting the drain so that the git2 ODB is not
    // churned by concurrent fast-import pack writes during the tree-walk.
    let t_tree_init = Instant::now();
    if !cli.quiet { println!("Initializing tree state..."); }
    let tree_state = tree_state::TreeState::new(git_source.repo(), &base_sha1, &patch_results)
        .context("initializing tree state from base commit")?;
    if !cli.quiet { eprintln!("  tree state init: {:.2}s", t_tree_init.elapsed().as_secs_f64()); }

    // Start blob drain in background (drops shard channels → all fast-imports see EOF).
    let t_drain_start = Instant::now();
    if !cli.quiet { println!("Draining blob shards (background)..."); }
    let drain_jh = std::thread::spawn(move || shard_pool.finish());

    // Build trees and commits in parallel with the drain.
    // The pack is buffered in memory (no disk writes) to avoid I/O contention
    // with the fast-import blob drain.  We flush to disk after drain completes.
    let t_commits = Instant::now();
    if !cli.quiet { println!("Building commits..."); }
    let (commit_pack, last_commit_oid) = build_commits_pack(
        &patch_results, &base_sha1, committer_timestamp, tree_state, &cli.output, cli.quiet,
    )?;
    if !cli.quiet { println!("Commits built in {:.1}s", t_commits.elapsed().as_secs_f64()); }

    // Wait for drain; with commit building doing no disk I/O, drain should now run
    // at full disk bandwidth (back to ~2s instead of the contended ~6s).
    let t_drain_wait = Instant::now();
    drain_jh.join().expect("blob drain thread panicked")
        .context("draining blob shards")?;
    if !cli.quiet {
        let waited = t_drain_wait.elapsed();
        let total = t_drain_start.elapsed();
        println!("Blob drain: {:.1}s total, waited {:.1}s", total.as_secs_f64(), waited.as_secs_f64());
    }

    // Flush commit pack to disk now that drain is done (no I/O contention).
    let t_pack_write = Instant::now();
    commit_pack.finish().context("finalizing commit pack")?;
    if !cli.quiet { println!("Commit pack written in {:.2}s", t_pack_write.elapsed().as_secs_f64()); }

    // Update branch ref now that both the commit pack and all blobs are in the ODB.
    let repo = git2::Repository::open(&cli.output).context("opening output repo")?;
    repo.reference(
        &format!("refs/heads/{}", cli.branch),
        last_commit_oid,
        true,
        "kexpand",
    ).context("updating branch ref")?;

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

/// Build all commits into an in-memory pack buffer.
/// Returns (pack_writer, last_commit_oid).  The caller must call `pack.finish()`
/// to flush to disk — ideally after the blob drain thread has completed so there
/// is no I/O contention.
fn build_commits_pack(
    results: &[PatchResult],
    base_commit_sha1: &str,
    committer_timestamp: i64,
    mut tree_state: TreeState,
    output: &Path,
    _quiet: bool,
) -> Result<(CommitPackWriter, git2::Oid)> {
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

    let last_oid = parent_oid.ok_or_else(|| anyhow::anyhow!("no commits written"))?;
    Ok((pack, last_oid))
}

/// Merge all local pack files into one pack in pure Rust — no subprocess, no
/// decompression.  We concatenate the raw pack bodies verbatim and rebuild the
/// pack index with updated absolute offsets.
///
/// Key insight: OFS_DELTA back-offsets in the commit pack encode distances
/// *between* objects within that pack.  When we insert the entire commit pack as
/// a contiguous block (preserving internal order), every pair (object, base)
/// shifts by the same absolute amount, so the encoded back-offset is unchanged.
/// We can therefore copy every source pack's body byte-for-byte.
///
/// merged_offset(pack_i, in_pack_off) = cumulative_body[i] + in_pack_off
/// where cumulative_body[0] = 0 and body_size = file_size − 32.
fn run_repack(output: &Path, _threads: usize) -> Result<()> {
    let pack_dir = output.join(".git/objects/pack");

    let idx_files: Vec<PathBuf> = std::fs::read_dir(&pack_dir)
        .context("reading pack dir")?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |e| e == "idx"))
        .collect();

    if idx_files.is_empty() {
        return Ok(());
    }

    // Read each pack's index and compute its body size.
    struct SourcePack {
        pack_path: PathBuf,
        idx_path: PathBuf,
        objects: Vec<(/* sha1 */ [u8; 20], /* crc32 */ u32, /* in_pack_offset */ u64)>,
        body_size: u64, // pack_file_size − 32
    }

    let mut sources: Vec<SourcePack> = Vec::with_capacity(idx_files.len());
    for idx_path in &idx_files {
        let pack_path = idx_path.with_extension("pack");
        let pack_size = std::fs::metadata(&pack_path)
            .with_context(|| format!("stat {}", pack_path.display()))?.len();
        if pack_size < 32 {
            bail!("pack file too small: {}", pack_path.display());
        }
        let objects = read_pack_index_v2(idx_path)
            .with_context(|| format!("reading index {}", idx_path.display()))?;
        sources.push(SourcePack {
            pack_path,
            idx_path: idx_path.clone(),
            objects,
            body_size: pack_size - 32,
        });
    }

    let total_objects: usize = sources.iter().map(|s| s.objects.len()).sum();

    // Write merged pack to a temp file (final name depends on its checksum).
    let tmp_pack = pack_dir.join("pack-merge.pack.tmp");
    let mut out = BufWriter::with_capacity(4 * 1024 * 1024,
        File::create(&tmp_pack).context("creating tmp pack")?);
    let mut hasher = openssl::sha::Sha1::new();

    // 12-byte pack header: "PACK" + version(2) + count
    let mut hdr = [0u8; 12];
    hdr[0..4].copy_from_slice(b"PACK");
    hdr[4..8].copy_from_slice(&2u32.to_be_bytes());
    hdr[8..12].copy_from_slice(&(total_objects as u32).to_be_bytes());
    hasher.update(&hdr);
    out.write_all(&hdr)?;

    // Compute cumulative body offsets; copy each pack body verbatim.
    let mut cum: Vec<u64> = Vec::with_capacity(sources.len());
    let mut running = 0u64;
    let mut copy_buf = vec![0u8; 4 * 1024 * 1024];
    for src in &sources {
        cum.push(running);
        let mut pack_file = File::open(&src.pack_path)
            .with_context(|| format!("opening {}", src.pack_path.display()))?;
        // Skip 12-byte header
        pack_file.seek(std::io::SeekFrom::Start(12)).context("seeking past pack header")?;
        // Copy body (body_size bytes, skipping trailing 20-byte checksum)
        let mut remaining = src.body_size;
        while remaining > 0 {
            let chunk = (remaining as usize).min(copy_buf.len());
            let n = pack_file.read(&mut copy_buf[..chunk]).context("reading pack body")?;
            if n == 0 { bail!("unexpected EOF in pack body"); }
            hasher.update(&copy_buf[..n]);
            out.write_all(&copy_buf[..n])?;
            remaining -= n as u64;
        }
        running += src.body_size;
    }

    // Trailing SHA-1 checksum of the merged pack.
    let pack_sha1: [u8; 20] = hasher.finish();
    out.write_all(&pack_sha1)?;
    out.flush()?;
    drop(out);

    // Build merged index: collect (sha1, crc32, merged_offset) for all objects.
    let mut all_objects: Vec<([u8; 20], u32, u64)> =
        Vec::with_capacity(total_objects);
    for (i, src) in sources.iter().enumerate() {
        let base = cum[i];
        for &(sha1, crc32, off) in &src.objects {
            all_objects.push((sha1, crc32, base + off));
        }
    }
    // Sort by SHA-1 (required by pack index v2).
    all_objects.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    let sha1_hex: String = pack_sha1.iter().map(|b| format!("{b:02x}")).collect();
    let final_pack = pack_dir.join(format!("pack-{sha1_hex}.pack"));
    let final_idx  = pack_dir.join(format!("pack-{sha1_hex}.idx"));

    write_merged_index(&all_objects, &pack_sha1, &final_idx)
        .context("writing merged index")?;
    std::fs::rename(&tmp_pack, &final_pack).context("renaming merged pack")?;

    // Remove source packs and indices.
    for src in &sources {
        std::fs::remove_file(&src.pack_path)
            .with_context(|| format!("removing {}", src.pack_path.display()))?;
        std::fs::remove_file(&src.idx_path)
            .with_context(|| format!("removing {}", src.idx_path.display()))?;
    }

    Ok(())
}

/// Read a pack index v2 file and return (sha1, crc32, in_pack_offset) for each object.
/// in_pack_offset is the absolute byte offset from the start of the pack file.
fn read_pack_index_v2(path: &Path) -> Result<Vec<([u8; 20], u32, u64)>> {
    let data = std::fs::read(path).context("reading pack index")?;
    if data.len() < 8 + 1024 {
        bail!("pack index too small");
    }
    if &data[0..4] != b"\xff\x74\x4f\x63" {
        bail!("not a pack index v2 (bad magic)");
    }
    if u32::from_be_bytes(data[4..8].try_into().unwrap()) != 2 {
        bail!("pack index is not version 2");
    }

    // Fan-out table: data[8..8+1024].  N = fan_out[255]
    let fanout_off = 8usize;
    let n = u32::from_be_bytes(data[fanout_off + 255*4..fanout_off + 256*4].try_into().unwrap()) as usize;

    let sha1s_off  = fanout_off + 1024;
    let crc32s_off = sha1s_off  + n * 20;
    let offsets_off = crc32s_off + n * 4;
    let large_off   = offsets_off + n * 4;

    if data.len() < large_off + 40 {
        bail!("pack index data truncated");
    }

    let mut result = Vec::with_capacity(n);
    for i in 0..n {
        let sha1: [u8; 20] = data[sha1s_off + i*20..sha1s_off + i*20 + 20].try_into().unwrap();
        let crc32 = u32::from_be_bytes(data[crc32s_off + i*4..crc32s_off + i*4 + 4].try_into().unwrap());
        let raw_off = u32::from_be_bytes(data[offsets_off + i*4..offsets_off + i*4 + 4].try_into().unwrap());
        let offset: u64 = if raw_off & 0x8000_0000 != 0 {
            // Large offset: index into the 8-byte large offset table
            let li = (raw_off & 0x7FFF_FFFF) as usize;
            let lo = large_off + li * 8;
            u64::from_be_bytes(data[lo..lo + 8].try_into().unwrap())
        } else {
            raw_off as u64
        };
        result.push((sha1, crc32, offset));
    }
    Ok(result)
}

/// Write a pack index v2 for the merged pack.
/// `objects` must already be sorted by SHA-1.
fn write_merged_index(
    objects: &[([u8; 20], u32, u64)],
    pack_sha1: &[u8; 20],
    path: &Path,
) -> Result<()> {
    let n = objects.len();
    let mut buf: Vec<u8> = Vec::with_capacity(8 + 1024 + n * 28 + 40);

    buf.extend_from_slice(b"\xff\x74\x4f\x63");
    buf.extend_from_slice(&2u32.to_be_bytes());

    let mut fanout = [0u32; 256];
    for (sha1, _, _) in objects {
        fanout[sha1[0] as usize] += 1;
    }
    let mut acc = 0u32;
    for v in &mut fanout {
        acc += *v;
        *v = acc;
    }
    for v in &fanout {
        buf.extend_from_slice(&v.to_be_bytes());
    }
    for (sha1, _, _) in objects {
        buf.extend_from_slice(sha1);
    }
    for (_, crc32, _) in objects {
        buf.extend_from_slice(&crc32.to_be_bytes());
    }

    let mut large_offsets: Vec<u64> = Vec::new();
    for (_, _, off) in objects {
        if *off > 0x7FFF_FFFF {
            let li = large_offsets.len() as u32;
            buf.extend_from_slice(&(0x8000_0000u32 | li).to_be_bytes());
            large_offsets.push(*off);
        } else {
            buf.extend_from_slice(&(*off as u32).to_be_bytes());
        }
    }
    for off in &large_offsets {
        buf.extend_from_slice(&off.to_be_bytes());
    }

    buf.extend_from_slice(pack_sha1);

    let mut idx_hasher = openssl::sha::Sha1::new();
    idx_hasher.update(&buf);
    buf.extend_from_slice(&idx_hasher.finish());

    let mut f = File::create(path).context("creating merged index")?;
    f.write_all(&buf).context("writing merged index")?;
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

use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::thread::JoinHandle;

use anyhow::{bail, Context, Result};

struct BlobShard {
    tx: SyncSender<(u64, Vec<u8>)>,
    writer_jh: JoinHandle<Result<()>>,
    child: Child,
    marks_file: PathBuf,
    next_mark: u64,
}

impl BlobShard {
    fn spawn(output_dir: &Path, idx: usize) -> Result<Self> {
        let marks_file = output_dir.join(format!(".git/blob-marks-{idx}.txt"));
        let mut child = Command::new("git")
            .args([
                "-c", "pack.compression=0",
                "fast-import",
                "--done",
                &format!("--export-marks={}", marks_file.display()),
            ])
            .current_dir(output_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning blob shard fast-import")?;

        let stdin = child.stdin.take().unwrap();
        let (tx, rx) = sync_channel::<(u64, Vec<u8>)>(512);

        let writer_jh = std::thread::spawn(move || -> Result<()> {
            let mut w = BufWriter::with_capacity(512 * 1024, stdin);
            for (mark, content) in rx {
                write!(w, "blob\nmark :{mark}\ndata {}\n", content.len())?;
                w.write_all(&content)?;
                w.write_all(b"\n")?;
            }
            // Channel closed — all blobs sent; finalize
            w.write_all(b"done\n")?;
            w.flush()?;
            Ok(())
        });

        Ok(BlobShard { tx, writer_jh, child, marks_file, next_mark: 1 })
    }

    fn emit_blob(&mut self, content: Vec<u8>) -> Result<u64> {
        let mark = self.next_mark;
        self.next_mark += 1;
        self.tx.send((mark, content))
            .map_err(|_| anyhow::anyhow!("blob shard writer thread died"))?;
        Ok(mark)
    }
}

pub struct BlobShardPool {
    shards: Vec<BlobShard>,
    next_shard: usize,
}

impl BlobShardPool {
    pub fn new(output_dir: &Path, n_shards: usize) -> Result<Self> {
        let n = n_shards.max(1);
        let shards = (0..n)
            .map(|i| BlobShard::spawn(output_dir, i))
            .collect::<Result<Vec<_>>>()?;
        Ok(BlobShardPool { shards, next_shard: 0 })
    }

    /// Assign blob to the next shard (round-robin). Content is moved (owned Vec<u8>).
    /// Returns `(shard_index, local_mark)`.
    pub fn emit_blob(&mut self, content: Vec<u8>) -> Result<(usize, u64)> {
        let idx = self.next_shard % self.shards.len();
        self.next_shard += 1;
        let mark = self.shards[idx].emit_blob(content)?;
        Ok((idx, mark))
    }

    pub fn n_shards(&self) -> usize {
        self.shards.len()
    }

    /// Drop all sender channels simultaneously so all writer threads see EOF at
    /// the same instant → all N fast-import shards drain in parallel.
    /// Waits for writer threads then child processes, reads and deletes marks files.
    /// Returns one `HashMap<local_mark, Oid>` per shard.
    pub fn finish_and_read_marks(self) -> Result<Vec<HashMap<u64, git2::Oid>>> {
        // Decompose before dropping anything
        let mut txs = Vec::new();
        let mut writer_jhs = Vec::new();
        let mut children = Vec::new();
        let mut marks_files = Vec::new();
        for shard in self.shards {
            txs.push(shard.tx);
            writer_jhs.push(shard.writer_jh);
            children.push(shard.child);
            marks_files.push(shard.marks_file);
        }

        // Drop all senders simultaneously → writer threads all see channel closed at once
        drop(txs);

        for jh in writer_jhs {
            jh.join().expect("writer thread panicked")?;
        }
        for mut child in children {
            let st = child.wait().context("waiting for blob shard fast-import")?;
            if !st.success() {
                bail!("blob shard fast-import exited with {st}");
            }
        }

        let maps = marks_files.iter().map(|p| read_marks_file(p)).collect::<Result<_>>()?;
        for path in &marks_files {
            let _ = std::fs::remove_file(path);
        }
        Ok(maps)
    }
}

fn read_marks_file(path: &Path) -> Result<HashMap<u64, git2::Oid>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening marks file {}", path.display()))?;
    let mut map = HashMap::new();
    for line in BufReader::new(file).lines() {
        let line = line.context("reading marks file")?;
        if let Some((mark_str, sha1)) = line.split_once(' ') {
            if let Some(mark) = mark_str.strip_prefix(':').and_then(|s| s.parse::<u64>().ok()) {
                if let Ok(oid) = git2::Oid::from_str(sha1.trim()) {
                    map.insert(mark, oid);
                }
            }
        }
    }
    Ok(map)
}

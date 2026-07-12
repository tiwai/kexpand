use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::thread::JoinHandle;

use anyhow::{bail, Context, Result};

struct BlobShard {
    tx: SyncSender<Vec<u8>>,
    writer_jh: JoinHandle<Result<()>>,
    child: Child,
}

impl BlobShard {
    fn spawn(output_dir: &Path) -> Result<Self> {
        let mut child = Command::new("git")
            .args(["-c", "pack.compression=0", "fast-import", "--done"])
            .current_dir(output_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning blob shard fast-import")?;

        let stdin = child.stdin.take().unwrap();
        let (tx, rx) = sync_channel::<Vec<u8>>(512);

        let writer_jh = std::thread::spawn(move || -> Result<()> {
            use std::io::{BufWriter, Write};
            let mut w = BufWriter::with_capacity(512 * 1024, stdin);
            for content in rx {
                write!(w, "blob\ndata {}\n", content.len())?;
                w.write_all(&content)?;
                w.write_all(b"\n")?;
            }
            // Channel closed — all blobs sent; finalize
            w.write_all(b"done\n")?;
            w.flush()?;
            Ok(())
        });

        Ok(BlobShard { tx, writer_jh, child })
    }

    fn emit_blob(&self, content: Vec<u8>) -> Result<()> {
        self.tx.send(content)
            .map_err(|_| anyhow::anyhow!("blob shard writer thread died"))
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
            .map(|_| BlobShard::spawn(output_dir))
            .collect::<Result<Vec<_>>>()?;
        Ok(BlobShardPool { shards, next_shard: 0 })
    }

    /// Route blob to the next shard (round-robin). Content is moved (owned Vec<u8>).
    /// The blob's SHA-1 is already known by the caller (computed at apply time).
    pub fn emit_blob(&mut self, content: Vec<u8>) -> Result<()> {
        let idx = self.next_shard % self.shards.len();
        self.next_shard += 1;
        self.shards[idx].emit_blob(content)
    }

    /// Drop all sender channels simultaneously so all writer threads see EOF at
    /// the same instant → all N fast-import shards drain in parallel.
    /// Waits for writer threads then child processes.
    pub fn finish(self) -> Result<()> {
        // Decompose before dropping anything
        let mut txs = Vec::new();
        let mut writer_jhs = Vec::new();
        let mut children = Vec::new();
        for shard in self.shards {
            txs.push(shard.tx);
            writer_jhs.push(shard.writer_jh);
            children.push(shard.child);
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

        Ok(())
    }
}

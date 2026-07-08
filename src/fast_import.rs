use std::io::{self, BufWriter, Write};
use std::path::Path;
use anyhow::Result;
use crate::patch_header::PatchMeta;

pub enum Parent {
    /// First commit: reference the base commit by its SHA1
    BaseSha1(String),
    /// All subsequent commits: reference by fast-import mark
    Mark(u64),
}

/// A reference to a blob in a commit tree entry.
#[derive(Clone)]
pub enum BlobRef {
    /// Blob already exists in the object store (e.g. via alternates); reference by SHA-1.
    Sha1(git2::Oid),
}

pub struct FastImport<W: Write> {
    out: BufWriter<W>,
    next_mark: u64,
}

impl<W: Write> FastImport<W> {
    pub fn new(writer: W) -> Self {
        FastImport {
            out: BufWriter::with_capacity(512 * 1024, writer),
            next_mark: 1,
        }
    }

    fn alloc_mark(&mut self) -> u64 {
        let m = self.next_mark;
        self.next_mark += 1;
        m
    }

    /// Emit a commit. Returns the commit's mark.
    ///
    /// `modified`: (path, blob_ref, mode) for each added/modified file.
    /// `deleted`:  paths to delete in this commit.
    pub fn emit_commit(
        &mut self,
        meta: &PatchMeta,
        branch: &str,
        parent: Parent,
        committer_timestamp: i64,
        modified: &[(&Path, BlobRef, u32)],
        deleted: &[&Path],
    ) -> Result<u64> {
        let mark = self.alloc_mark();
        let out = &mut self.out;

        write!(out, "commit refs/heads/{branch}\nmark :{mark}\n")?;

        // Author: sanitize name and email for git fast-import ident format
        let raw_name = if meta.author_name.is_empty() { "Unknown" } else { &meta.author_name };
        let raw_email = if meta.author_email.is_empty() { "unknown@unknown" } else { &meta.author_email };
        // Strip any < or > that would break the ident format
        let name: String = raw_name.chars().filter(|&c| c != '<' && c != '>').collect();
        let email: String = raw_email.chars().filter(|&c| c != '<' && c != '>').collect();
        let ts = meta.author_timestamp;
        let tz = &meta.author_tz;
        write!(out, "author {name} <{email}> {ts} {tz}\n")?;
        write!(out, "committer kexpand <kexpand@localhost> {committer_timestamp} +0000\n")?;

        // Message
        let msg = build_message(meta);
        write!(out, "data {}\n", msg.len())?;
        out.write_all(msg.as_bytes())?;
        out.write_all(b"\n")?;

        // Parent reference
        match &parent {
            Parent::BaseSha1(sha1) => write!(out, "from {sha1}\n")?,
            Parent::Mark(m) => write!(out, "from :{m}\n")?,
        }

        // File modifications and deletions
        for (path, blob_ref, mode) in modified {
            let path_str = path.to_str().unwrap_or("");
            let BlobRef::Sha1(oid) = blob_ref;
            write!(out, "M {:06o} {oid} {path_str}\n", mode)?;
        }
        for path in deleted {
            let path_str = path.to_str().unwrap_or("");
            write!(out, "D {path_str}\n")?;
        }
        out.write_all(b"\n")?;

        Ok(mark)
    }

    /// Write the `done` command and flush. Must be called before dropping
    /// when git fast-import was invoked with `--done`.
    pub fn done(&mut self) -> io::Result<()> {
        self.out.write_all(b"done\n")?;
        self.out.flush()
    }
}

fn build_message(meta: &PatchMeta) -> String {
    let subject = if meta.subject.is_empty() {
        meta.source_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string()
    } else {
        meta.subject.clone()
    };

    let mut msg = subject;
    if !meta.body.is_empty() {
        msg.push_str("\n\n");
        msg.push_str(&meta.body);
    }
    let mut extras = String::new();
    if !meta.references.is_empty() {
        extras.push_str(&format!("References: {}\n", meta.references));
    }
    if !meta.patch_mainline.is_empty() {
        extras.push_str(&format!("Patch-mainline: {}\n", meta.patch_mainline));
    }
    if !extras.is_empty() {
        msg.push_str("\n\n");
        msg.push_str(&extras);
    }
    msg
}

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use openssl::sha::Sha1;

// ─── CRC32 (ISO 3309 / zlib polynomial) ──────────────────────────────────────

const fn make_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut j = 0usize;
        while j < 8 {
            c = if c & 1 != 0 { 0xEDB88320 ^ (c >> 1) } else { c >> 1 };
            j += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC32_TABLE: [u32; 256] = make_crc32_table();

fn crc32_update(state: u32, data: &[u8]) -> u32 {
    let mut crc = state;
    for &b in data {
        crc = CRC32_TABLE[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

fn crc32_of(a: &[u8], b: &[u8]) -> u32 {
    !crc32_update(crc32_update(!0u32, a), b)
}

// ─── PackObjectRecord ─────────────────────────────────────────────────────────

struct PackObjectRecord {
    sha1: [u8; 20],
    offset: u64,
    crc32: u32,
}

// ─── CommitPackWriter ─────────────────────────────────────────────────────────

/// Writes a git pack file v2 containing tree and commit objects (no blobs, no deltas).
/// Call `write_object()` for each tree/commit, then `finish()` to seal the file
/// and write the companion .idx — no external process needed.
pub struct CommitPackWriter {
    writer: BufWriter<File>,
    n_objects: u32,
    path: PathBuf,
    objects: Vec<PackObjectRecord>,
    current_offset: u64,
    seen: HashSet<git2::Oid>,
}

impl CommitPackWriter {
    pub fn create(pack_dir: &Path) -> Result<Self> {
        let path = pack_dir.join("pack-wip.pack");
        let file = File::create(&path).context("creating commit pack")?;
        let mut writer = BufWriter::with_capacity(4 * 1024 * 1024, file);
        // Pack header: magic + version 2 + placeholder object count
        writer.write_all(b"PACK\x00\x00\x00\x02\x00\x00\x00\x00")?;
        Ok(CommitPackWriter {
            writer,
            n_objects: 0,
            path,
            objects: Vec::with_capacity(400_000),
            current_offset: 12, // 4 (magic) + 4 (version) + 4 (count) = 12
            seen: HashSet::with_capacity(400_000),
        })
    }

    /// Write one raw git object.  `obj_type`: 1 = commit, 4 = tree.
    /// Returns the git object SHA-1 (computed via OpenSSL SHA-NI).
    pub fn write_object(&mut self, obj_type: u8, content: &[u8]) -> Result<git2::Oid> {
        let type_name = match obj_type {
            1 => "commit",
            2 => "tree",
            _ => bail!("unexpected obj_type {obj_type}"),
        };

        // git SHA-1: sha1("type SIZE\0content")
        let git_hdr = format!("{type_name} {}\0", content.len());
        let mut hasher = Sha1::new();
        hasher.update(git_hdr.as_bytes());
        hasher.update(content);
        let sha1 = hasher.finish();
        let oid = git2::Oid::from_bytes(&sha1).expect("SHA-1 is 20 bytes");

        // Skip duplicates: same content → same OID → already in the pack.
        if !self.seen.insert(oid) {
            return Ok(oid);
        }

        // Pack object bytes: type+size varint header, then zlib-stored content
        let header = pack_type_size(obj_type, content.len());
        let zlib = zlib_store(content);

        // CRC32 over pack bytes for this object (git pack index requires it)
        let crc = crc32_of(&header, &zlib);

        let offset = self.current_offset;
        self.objects.push(PackObjectRecord { sha1, offset, crc32: crc });

        self.writer.write_all(&header)?;
        self.writer.write_all(&zlib)?;
        self.current_offset += (header.len() + zlib.len()) as u64;
        self.n_objects += 1;

        Ok(oid)
    }

    /// Seal the pack: patch the object count, append the 20-byte pack SHA-1,
    /// write the companion .idx file in Rust (no subprocess), then rename both
    /// to canonical `pack-{sha1}.{pack,idx}` names.
    pub fn finish(mut self) -> Result<()> {
        self.writer.flush()?;
        drop(self.writer);

        // Patch object count at byte offset 8
        {
            let mut f = OpenOptions::new().write(true).open(&self.path)?;
            f.seek(SeekFrom::Start(8))?;
            f.write_all(&self.n_objects.to_be_bytes())?;
        }

        // Compute SHA-1 over the entire file (header + objects, correct count)
        let pack_sha1: [u8; 20] = {
            let mut f = File::open(&self.path)?;
            let mut hasher = Sha1::new();
            let mut buf = [0u8; 65536];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 { break; }
                hasher.update(&buf[..n]);
            }
            hasher.finish()
        };

        // Append 20-byte pack checksum
        OpenOptions::new().append(true).open(&self.path)?
            .write_all(&pack_sha1)?;

        // Canonical names derived from pack SHA-1
        let sha1_hex: String = pack_sha1.iter().map(|b| format!("{b:02x}")).collect();
        let pack_dir = self.path.parent().unwrap();
        let canon_pack = pack_dir.join(format!("pack-{sha1_hex}.pack"));
        let canon_idx  = pack_dir.join(format!("pack-{sha1_hex}.idx"));

        // Write pack index v2 entirely in Rust (saves ~3.8s vs git index-pack)
        write_pack_index(&self.objects, &pack_sha1, &canon_idx)
            .context("writing pack index")?;

        // Rename pack to canonical name
        std::fs::rename(&self.path, &canon_pack)
            .context("renaming pack to canonical name")?;

        Ok(())
    }
}

// ─── Pack index v2 writer ─────────────────────────────────────────────────────

fn write_pack_index(
    objects: &[PackObjectRecord],
    pack_sha1: &[u8; 20],
    index_path: &Path,
) -> Result<()> {
    let n = objects.len();

    // Sort by SHA-1 (index requires lexicographic order)
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_unstable_by(|&a, &b| objects[a].sha1.cmp(&objects[b].sha1));

    // Fan-out table: fanout[i] = count of objects with sha1[0] <= i (cumulative)
    let mut fanout = [0u32; 256];
    for &idx in &order {
        fanout[objects[idx].sha1[0] as usize] += 1;
    }
    let mut acc = 0u32;
    for entry in &mut fanout {
        acc += *entry;
        *entry = acc;
    }

    // Buffer entire index in memory before writing (avoids many small writes)
    // Size: 8 + 256*4 + n*(20+4+4) + (large offsets)*8 + 20 + 20
    let mut buf: Vec<u8> = Vec::with_capacity(8 + 1024 + n * 28 + 40);

    // Magic + version 2
    buf.extend_from_slice(b"\xff\x74\x4f\x63");
    buf.extend_from_slice(&2u32.to_be_bytes());

    // Fan-out table (256 × u32 big-endian)
    for &v in &fanout {
        buf.extend_from_slice(&v.to_be_bytes());
    }

    // SHA-1 table (sorted)
    for &idx in &order {
        buf.extend_from_slice(&objects[idx].sha1);
    }

    // CRC32 table (same order)
    for &idx in &order {
        buf.extend_from_slice(&objects[idx].crc32.to_be_bytes());
    }

    // Offset table: 4-byte entry per object.
    // If offset > 2^31-1 the high bit is set and the value indexes the large table.
    let mut large_offsets: Vec<u64> = Vec::new();
    for &idx in &order {
        let off = objects[idx].offset;
        if off > 0x7FFF_FFFF {
            let li = large_offsets.len() as u32;
            buf.extend_from_slice(&(0x8000_0000 | li).to_be_bytes());
            large_offsets.push(off);
        } else {
            buf.extend_from_slice(&(off as u32).to_be_bytes());
        }
    }

    // Large offset table (8-byte big-endian, only if any offsets > 2 GiB)
    for &off in &large_offsets {
        buf.extend_from_slice(&off.to_be_bytes());
    }

    // Pack SHA-1
    buf.extend_from_slice(pack_sha1);

    // Index SHA-1 (SHA-1 of everything written above)
    let mut idx_hasher = Sha1::new();
    idx_hasher.update(&buf);
    buf.extend_from_slice(&idx_hasher.finish());

    let mut f = File::create(index_path).context("creating index file")?;
    f.write_all(&buf).context("writing index file")?;

    Ok(())
}

// ─── Pack format helpers ──────────────────────────────────────────────────────

/// Encode the pack object type+size header (variable-length).
/// Format: first byte = [more:1][type:3][size_lo4:4], subsequent bytes [more:1][size_7:7].
fn pack_type_size(obj_type: u8, size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    let mut rem = size >> 4;
    let first = (obj_type << 4) | (size & 0xF) as u8;
    out.push(if rem > 0 { 0x80 | first } else { first });
    while rem > 0 {
        let b = (rem & 0x7F) as u8;
        rem >>= 7;
        out.push(if rem > 0 { 0x80 | b } else { b });
    }
    out
}

// ─── zlib stored (level 0) ───────────────────────────────────────────────────

/// Wrap `data` in zlib stored format (RFC 1950 + RFC 1951 BTYPE=00).
/// No external dependencies: header, stored blocks, Adler-32 checksum.
fn zlib_store(data: &[u8]) -> Vec<u8> {
    // CMF=0x78 (CM=8, CINFO=7 → 32 K window)
    // FLG=0x01: FLEVEL=0, FDICT=0, FCHECK=1 → (0x78*256 + 0x01) % 31 == 0 ✓
    let n_blocks = data.len() / 65535 + 1;
    let mut out = Vec::with_capacity(data.len() + 6 + n_blocks * 5);
    out.extend_from_slice(&[0x78, 0x01]);

    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);
    } else {
        let mut pos = 0;
        while pos < data.len() {
            let end = (pos + 65535).min(data.len());
            let len = (end - pos) as u16;
            out.push(if end == data.len() { 0x01 } else { 0x00 });
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(&data[pos..end]);
            pos = end;
        }
    }

    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    // Process in chunks of 5552 bytes — the max safe size without u32 overflow.
    // This reduces modulo operations from 2N to 2*(N/5552), roughly 5500× fewer.
    let (mut s1, mut s2) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &b in chunk {
            s1 += b as u32;
            s2 += s1;
        }
        s1 %= MOD;
        s2 %= MOD;
    }
    (s2 << 16) | s1
}

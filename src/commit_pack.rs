use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Write;
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

fn crc32_of_slices(slices: &[&[u8]]) -> u32 {
    let mut state = !0u32;
    for s in slices { state = crc32_update(state, s); }
    !state
}

// ─── PackObjectRecord ─────────────────────────────────────────────────────────

struct PackObjectRecord {
    sha1: [u8; 20],
    offset: u64,
    crc32: u32,
}

// ─── Tree delta cache entry ───────────────────────────────────────────────────

struct TreeCacheEntry {
    content: Vec<u8>,   // uncompressed tree bytes of the previous version
    offset: u64,        // pack byte offset of that version
    depth: u32,         // delta chain depth (0 = full object)
}

const MAX_DELTA_DEPTH: u32 = 50;

// ─── CommitPackWriter ─────────────────────────────────────────────────────────

/// Writes a git pack file v2 containing tree and commit objects (no blobs, no deltas).
/// The entire pack is accumulated in an in-memory buffer to avoid disk I/O during
/// the concurrent blob-shard drain phase.  Call `finish()` to flush to disk and
/// write the companion .idx — no external process needed.
pub struct CommitPackWriter {
    buf: Vec<u8>,
    n_objects: u32,
    pack_dir: PathBuf,
    objects: Vec<PackObjectRecord>,
    current_offset: u64,
    seen: HashSet<git2::Oid>,
    /// Per-path cache for tree delta compression.
    tree_cache: HashMap<PathBuf, TreeCacheEntry>,
}

impl CommitPackWriter {
    pub fn create(pack_dir: &Path) -> Result<Self> {
        // Pre-allocate ~160 MiB: 660 K tree+commit objects × ~250 bytes average.
        let mut buf = Vec::with_capacity(160 * 1024 * 1024);
        // Pack header: magic + version 2 + placeholder object count
        buf.extend_from_slice(b"PACK\x00\x00\x00\x02\x00\x00\x00\x00");
        Ok(CommitPackWriter {
            buf,
            n_objects: 0,
            pack_dir: pack_dir.to_path_buf(),
            objects: Vec::with_capacity(700_000),
            current_offset: 12, // 4 (magic) + 4 (version) + 4 (count) = 12
            seen: HashSet::with_capacity(700_000),
            tree_cache: HashMap::new(),
        })
    }

    /// Write a tree object, using OFS_DELTA against the previous version of the same
    /// path when possible.  Path tracking enables delta compression without needing
    /// git pack-objects to re-sort by filename.
    pub fn write_tree(&mut self, content: &[u8], path: &Path) -> Result<git2::Oid> {
        let oid = self.compute_oid(2, content);

        if !self.seen.insert(oid) {
            // Already in pack — still update cache so subsequent versions can delta off it.
            // (The existing entry at its original offset is the base.)
            // Only update if the cache entry is stale or absent.
            self.tree_cache.entry(path.to_path_buf()).or_insert_with(|| {
                // We don't know the offset for the deduped OID; fall back to full object
                // next time (depth 0, offset = current as sentinel — won't be used since
                // the next write_tree for this path won't enter the dedup branch).
                TreeCacheEntry { content: content.to_vec(), offset: 0, depth: 0 }
            });
            return Ok(oid);
        }

        // Try OFS_DELTA against the previous version of this path.
        let mut try_delta_info = None;
        if let Some(entry) = self.tree_cache.get(path) {
            if entry.depth < MAX_DELTA_DEPTH && entry.offset > 0 {
                let delta = git_delta_encode(&entry.content, content);
                // Use delta if it's meaningfully smaller than stored-format full object.
                // Stored format overhead: 2 (zlib hdr) + 5 (block hdr) per 65535 bytes + 4 (adler32)
                let full_stored_size = content.len() + 11
                    + (content.len() / 65535) * 5;
                if delta.len() < full_stored_size {
                    try_delta_info = Some((delta, entry.offset, entry.depth));
                }
            }
        }

        let wrote_delta = if let Some((delta, base_offset, base_depth)) = try_delta_info {
            let back_offset = self.current_offset - base_offset;
            self.write_ofs_delta(&delta, back_offset, oid, content);
            let entry = self.tree_cache.get_mut(path).unwrap();
            entry.content = content.to_vec();
            entry.offset = self.objects.last().unwrap().offset;
            entry.depth = base_depth + 1;
            true
        } else {
            false
        };

        if !wrote_delta {
            self.write_full_tree(content, oid);
            let offset = self.objects.last().unwrap().offset;
            self.tree_cache.insert(path.to_path_buf(), TreeCacheEntry {
                content: content.to_vec(),
                offset,
                depth: 0,
            });
        }

        Ok(oid)
    }

    /// Write one raw git object (commit or tree) as a full (non-delta) object.
    /// `obj_type`: 1 = commit, 2 = tree.
    pub fn write_object(&mut self, obj_type: u8, content: &[u8]) -> Result<git2::Oid> {
        let type_name = match obj_type {
            1 => "commit",
            2 => "tree",
            _ => bail!("unexpected obj_type {obj_type}"),
        };

        let git_hdr = format!("{type_name} {}\0", content.len());
        let mut hasher = Sha1::new();
        hasher.update(git_hdr.as_bytes());
        hasher.update(content);
        let sha1 = hasher.finish();
        let oid = git2::Oid::from_bytes(&sha1).expect("SHA-1 is 20 bytes");

        if !self.seen.insert(oid) {
            return Ok(oid);
        }

        let header = pack_type_size(obj_type, content.len());
        let start = self.buf.len();
        self.buf.extend_from_slice(&header);
        let zlib_start = self.buf.len();
        zlib_store_into(&mut self.buf, content);
        let crc = crc32_of_slices(&[&header, &self.buf[zlib_start..]]);
        self.objects.push(PackObjectRecord { sha1, offset: self.current_offset, crc32: crc });
        self.current_offset += (self.buf.len() - start) as u64;
        self.n_objects += 1;

        Ok(oid)
    }

    /// Flush the in-memory pack to disk: patch the object count, append the 20-byte
    /// pack SHA-1, write the companion .idx, then rename to canonical names.
    pub fn finish(mut self) -> Result<()> {
        self.buf[8..12].copy_from_slice(&self.n_objects.to_be_bytes());

        let pack_sha1: [u8; 20] = {
            let mut hasher = Sha1::new();
            hasher.update(&self.buf);
            hasher.finish()
        };
        self.buf.extend_from_slice(&pack_sha1);

        let sha1_hex: String = pack_sha1.iter().map(|b| format!("{b:02x}")).collect();
        let canon_pack = self.pack_dir.join(format!("pack-{sha1_hex}.pack"));
        let canon_idx  = self.pack_dir.join(format!("pack-{sha1_hex}.idx"));

        write_pack_index(&self.objects, &pack_sha1, &canon_idx)
            .context("writing pack index")?;

        let mut f = File::create(&canon_pack).context("creating commit pack")?;
        f.write_all(&self.buf).context("writing commit pack")?;

        Ok(())
    }

    // ─── Internal helpers ────────────────────────────────────────────────────

    fn compute_oid(&self, obj_type: u8, content: &[u8]) -> git2::Oid {
        let type_name = if obj_type == 1 { "commit" } else { "tree" };
        let git_hdr = format!("{type_name} {}\0", content.len());
        let mut hasher = Sha1::new();
        hasher.update(git_hdr.as_bytes());
        hasher.update(content);
        git2::Oid::from_bytes(&hasher.finish()).expect("SHA-1 is 20 bytes")
    }

    fn write_full_tree(&mut self, content: &[u8], oid: git2::Oid) {
        let sha1: [u8; 20] = oid.as_bytes().try_into().expect("20 bytes");
        let header = pack_type_size(2, content.len());
        let start = self.buf.len();
        self.buf.extend_from_slice(&header);
        let zlib_start = self.buf.len();
        zlib_store_into(&mut self.buf, content);
        let crc = crc32_of_slices(&[&header, &self.buf[zlib_start..]]);
        self.objects.push(PackObjectRecord { sha1, offset: self.current_offset, crc32: crc });
        self.current_offset += (self.buf.len() - start) as u64;
        self.n_objects += 1;
    }

    fn write_ofs_delta(&mut self, delta: &[u8], back_offset: u64, oid: git2::Oid, _content: &[u8]) {
        let sha1: [u8; 20] = oid.as_bytes().try_into().expect("20 bytes");

        // OFS_DELTA header: type=6, size=delta.len()
        let type_size_hdr = pack_type_size(6, delta.len());

        // Encode the back-offset (distance from current object to its base in pack)
        let ofs_bytes = encode_ofs_back(back_offset);

        // The delta bytes are zlib-stored (no actual compression — deltas are already small)
        let obj_start = self.buf.len();
        self.buf.extend_from_slice(&type_size_hdr);
        self.buf.extend_from_slice(&ofs_bytes);
        let zlib_start = self.buf.len();
        zlib_store_into(&mut self.buf, delta);

        let crc = crc32_of_slices(&[&type_size_hdr, &ofs_bytes, &self.buf[zlib_start..]]);
        self.objects.push(PackObjectRecord { sha1, offset: self.current_offset, crc32: crc });
        self.current_offset += (self.buf.len() - obj_start) as u64;
        self.n_objects += 1;
    }
}

// ─── OFS_DELTA back-offset encoding ──────────────────────────────────────────

/// Encode the OFS_DELTA back-offset in git's big-endian MSB-first variable-length format.
/// Decode: val = 0; for each byte in order: val = (val << 7) | (b & 0x7f); if !last: val += 1
fn encode_ofs_back(mut ofs: u64) -> Vec<u8> {
    let mut tmp = [0u8; 10];
    let mut len = 0usize;
    tmp[len] = (ofs & 0x7F) as u8;
    len += 1;
    ofs >>= 7;
    while ofs > 0 {
        ofs -= 1;
        tmp[len] = 0x80 | (ofs & 0x7F) as u8;
        len += 1;
        ofs >>= 7;
    }
    // Written MSB-first
    let mut out = vec![0u8; len];
    for i in 0..len {
        out[i] = tmp[len - 1 - i];
    }
    out
}

// ─── Git delta encoding ───────────────────────────────────────────────────────

thread_local! {
    static DELTA_TABLE_SCRATCH: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Produce a git-format binary delta that transforms `src` into `dst`.
/// Output: src_size (varint) || dst_size (varint) || instructions
///
/// Instructions:
///   COPY:   0x80 | flags  [offset bytes...] [size bytes...]
///   INSERT: 0x01..0x7F   followed by that many literal bytes
///
/// Uses a fixed-size direct-indexed table (Vec<u32>) for O(1) lookups — no
/// HashMap allocation.  Last-write-wins for hash collisions; extend match forward.
fn git_delta_encode(src: &[u8], dst: &[u8]) -> Vec<u8> {
    const BLOCK: usize = 16;

    // Power-of-two table sized to ~src.len()/4; minimum 256 slots.
    let tsize = (src.len() / 4 + 1).next_power_of_two().max(256);
    let tmask = (tsize - 1) as u32;

    DELTA_TABLE_SCRATCH.with(|scratch| {
        let mut table_borrow = scratch.borrow_mut();
        if table_borrow.len() < tsize {
            table_borrow.resize(tsize, 0);
        } else {
            table_borrow[..tsize].fill(0);
        }
        let table = &mut table_borrow[..tsize];

        // Build: stride=1, hash each BLOCK-byte window.  Last-write-wins on collision.
        if src.len() >= BLOCK {
            for i in 0..=src.len() - BLOCK {
                let h = hash16(&src[i..i + BLOCK]) & tmask;
                table[h as usize] = (i as u32) + 1;
            }
        }

        let mut out = Vec::with_capacity(dst.len() / 4 + 32);
        write_delta_varint(&mut out, src.len());
        write_delta_varint(&mut out, dst.len());

        let mut dp = 0usize;
        let mut insert_buf: Vec<u8> = Vec::with_capacity(128);

        while dp < dst.len() {
            let remaining = dst.len() - dp;
            let mut matched = None;

            if remaining >= BLOCK {
                let h = hash16(&dst[dp..dp + BLOCK]) & tmask;
                let slot = table[h as usize];
                if slot > 0 {
                    let sp = (slot - 1) as usize;
                    if sp + BLOCK <= src.len() && src[sp..sp + BLOCK] == dst[dp..dp + BLOCK] {
                        let max_fwd = (src.len() - sp).min(dst.len() - dp).min(0xFFFF);
                        let mut len = BLOCK;
                        while len < max_fwd && src[sp + len] == dst[dp + len] {
                            len += 1;
                        }
                        matched = Some((sp, len));
                    }
                }
            }

            match matched {
                Some((sp, len)) => {
                    flush_insert_buf(&mut out, &mut insert_buf);
                    emit_copy(&mut out, sp as u64, len as u64);
                    dp += len;
                }
                None => {
                    insert_buf.push(dst[dp]);
                    if insert_buf.len() == 127 {
                        flush_insert_buf(&mut out, &mut insert_buf);
                    }
                    dp += 1;
                }
            }
        }
        flush_insert_buf(&mut out, &mut insert_buf);
        out
    })
}

fn hash16(data: &[u8]) -> u32 {
    let mut h: u32 = 0x12345678;
    for &b in data {
        h = h.wrapping_mul(31).wrapping_add(b as u32);
    }
    h
}

fn write_delta_varint(out: &mut Vec<u8>, mut v: usize) {
    loop {
        if v < 0x80 { out.push(v as u8); break; }
        out.push((v & 0x7F) as u8 | 0x80);
        v >>= 7;
    }
}

fn flush_insert_buf(out: &mut Vec<u8>, buf: &mut Vec<u8>) {
    if buf.is_empty() { return; }
    let mut pos = 0;
    while pos < buf.len() {
        let n = (buf.len() - pos).min(127);
        out.push(n as u8);
        out.extend_from_slice(&buf[pos..pos + n]);
        pos += n;
    }
    buf.clear();
}

fn emit_copy(out: &mut Vec<u8>, offset: u64, size: u64) {
    let o = offset;
    let s = size;
    let mut flags = 0u8;
    let mut extra = [0u8; 7];
    let mut n = 0usize;
    for i in 0..4u64 {
        let b = ((o >> (i * 8)) & 0xFF) as u8;
        if b != 0 { flags |= 1 << i; extra[n] = b; n += 1; }
    }
    // Size: 0 means 0x10000 (65536); for sizes < 65536, encode normally
    if s != 65536 {
        for i in 0..3u64 {
            let b = ((s >> (i * 8)) & 0xFF) as u8;
            if b != 0 { flags |= 1 << (4 + i); extra[n] = b; n += 1; }
        }
    }
    // If s == 65536, leave size bits as 0 → git interprets as 65536 (see pack-format)
    out.push(0x80 | flags);
    out.extend_from_slice(&extra[..n]);
}

// ─── Pack index v2 writer ─────────────────────────────────────────────────────

fn write_pack_index(
    objects: &[PackObjectRecord],
    pack_sha1: &[u8; 20],
    index_path: &Path,
) -> Result<()> {
    let n = objects.len();

    let mut order: Vec<usize> = (0..n).collect();
    order.sort_unstable_by(|&a, &b| objects[a].sha1.cmp(&objects[b].sha1));

    let mut fanout = [0u32; 256];
    for &idx in &order {
        fanout[objects[idx].sha1[0] as usize] += 1;
    }
    let mut acc = 0u32;
    for entry in &mut fanout {
        acc += *entry;
        *entry = acc;
    }

    let mut buf: Vec<u8> = Vec::with_capacity(8 + 1024 + n * 28 + 40);

    buf.extend_from_slice(b"\xff\x74\x4f\x63");
    buf.extend_from_slice(&2u32.to_be_bytes());

    for &v in &fanout {
        buf.extend_from_slice(&v.to_be_bytes());
    }
    for &idx in &order {
        buf.extend_from_slice(&objects[idx].sha1);
    }
    for &idx in &order {
        buf.extend_from_slice(&objects[idx].crc32.to_be_bytes());
    }

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

    for &off in &large_offsets {
        buf.extend_from_slice(&off.to_be_bytes());
    }

    buf.extend_from_slice(pack_sha1);

    let mut idx_hasher = Sha1::new();
    idx_hasher.update(&buf);
    buf.extend_from_slice(&idx_hasher.finish());

    let mut f = File::create(index_path).context("creating index file")?;
    f.write_all(&buf).context("writing index file")?;

    Ok(())
}

// ─── Pack format helpers ──────────────────────────────────────────────────────

/// Encode the pack object type+size header (variable-length).
/// type values: 1=commit, 2=tree, 4=tag, 6=OFS_DELTA, 7=REF_DELTA
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

/// Append `data` in zlib stored format (RFC 1950 + RFC 1951 BTYPE=00) to `out`.
fn zlib_store_into(out: &mut Vec<u8>, data: &[u8]) {
    let n_blocks = if data.is_empty() { 1 } else { data.len().div_ceil(65535) };
    out.reserve(data.len() + 6 + n_blocks * 5);
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
}

fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
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

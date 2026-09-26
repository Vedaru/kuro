use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::utils::checksum::Checksum;
use crate::utils::compression::IO_BUF_SIZE;
use crate::utils::types::{ChecksumMode, CombinedStream};

const BLOCK: usize = 4096;
const MAX_CANDIDATES: usize = 4;

pub(crate) struct KrCreateOptions {
    pub compress: bool,
}

impl Default for KrCreateOptions {
    fn default() -> Self {
        Self { compress: true }
    }
}

struct Entry {
    path: String,
    size: u64,
}

fn walk_tree(root: &Path) -> io::Result<(Vec<String>, Vec<Entry>)> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            if path.is_dir() {
                dirs.push(format!("{}/", rel));
                stack.push(path);
            } else {
                files.push(Entry { path: rel, size: entry.metadata()?.len() });
            }
        }
    }

    dirs.sort();
    files.sort_by(|a, b| a.path.cmp(&b.path));

    // The root is an empty entry, then directories, then files. Their relative
    // order fixes the index list the head encodes.
    let mut paths = vec![String::new()];
    paths.extend(dirs);
    let file_start = paths.len();
    paths.extend(files.iter().map(|f| f.path.clone()));

    let indices: Vec<u64> = (0..files.len()).map(|i| (file_start + i) as u64).collect();
    debug_assert_eq!(indices.len(), files.len());
    Ok((paths, files))
}

fn file_indices(paths: &[String], files: &[Entry]) -> Vec<u64> {
    let start = paths.len() - files.len();
    (0..files.len()).map(|i| (start + i) as u64).collect()
}

fn pack_uint(out: &mut Vec<u8>, v: u64) {
    let mut groups = Vec::new();
    groups.push((v & 0x7F) as u8);
    let mut v = v >> 7;
    while v > 0 {
        groups.push((v & 0x7F) as u8);
        v >>= 7;
    }
    groups.reverse();
    let last = groups.len() - 1;
    for (i, g) in groups.iter().enumerate() {
        out.push(if i < last { g | 0x80 } else { *g });
    }
}

fn pack_signed(out: &mut Vec<u8>, v: i64) {
    let neg = v < 0;
    let mag = v.unsigned_abs();

    let mut k = 0usize;
    while 6 + 7 * (k + 1) <= 64 && (mag >> (6 + 7 * k)) != 0 { k += 1; }

    let mut first = ((mag >> (7 * k)) & 0x3F) as u8;
    if neg { first |= 0x80; }
    if k > 0 { first |= 0x40; }
    out.push(first);

    for i in (0..k).rev() {
        let mut b = ((mag >> (7 * i)) & 0x7F) as u8;
        if i > 0 { b |= 0x80; }
        out.push(b);
    }
}

fn fadler64(data: &[u8], state: &mut Option<Checksum>) {
    if let Some(c) = state.as_mut() { c.append(data); }
}

fn finish_fadler64(state: Option<Checksum>) -> u64 {
    match state {
        Some(c) => {
            let bytes = c.finish();
            let mut v = [0u8; 8];
            v.copy_from_slice(&bytes[..8]);
            u64::from_le_bytes(v)
        }
        None => 0,
    }
}

fn digest_file(path: &Path) -> io::Result<u64> {
    let mut state = ChecksumMode::Fadler64.new_checksum();
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; IO_BUF_SIZE];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 { break; }
        fadler64(&buf[..n], &mut state);
    }
    Ok(finish_fadler64(state))
}

fn combine_fadler64(x: u64, y: u64, len_y: u64) -> u64 {
    const M32: u64 = 0xFFFF_FFFF;
    let (ax, sx) = (x & M32, (x >> 32) & M32);
    let (ay, sy) = (y & M32, (y >> 32) & M32);
    let a = ax.wrapping_add(ay).wrapping_sub(1) & M32;
    let s = sx.wrapping_add(sy).wrapping_add(len_y.wrapping_mul(ax.wrapping_sub(1))) & M32;
    a | (s << 32)
}

const ADLER_INITIAL: u64 = 1;

fn fold_fadler64(digests: &[u64], sizes: &[u64]) -> u64 {
    let mut acc = ADLER_INITIAL;
    for (d, len) in digests.iter().zip(sizes) { acc = combine_fadler64(acc, *d, *len); }
    acc
}

struct Cover {
    old_pos: u64,
    new_gap: u64,
    length: u64,
}

struct OldIndex {
    table: HashMap<u64, Vec<u64>>,
}

fn hash_block(b: &[u8]) -> u64 {
    // FNV-1a: cheap, and collisions are resolved by an explicit byte compare.
    let mut h: u64 = 0xcbf29ce484222325;
    for &c in b {
        h ^= c as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

impl OldIndex {
    fn build(old: &mut CombinedStream, old_size: u64) -> io::Result<Self> {
        let mut table: HashMap<u64, Vec<u64>> = HashMap::new();
        let mut buf = vec![0u8; BLOCK];
        let mut pos = 0u64;
        old.seek(SeekFrom::Start(0))?;
        while pos + BLOCK as u64 <= old_size {
            old.read_exact(&mut buf)?;
            let entry = table.entry(hash_block(&buf)).or_default();
            if entry.len() < MAX_CANDIDATES { entry.push(pos); }
            pos += BLOCK as u64;
        }
        Ok(Self { table })
    }

    fn candidates(&self, h: u64) -> &[u64] {
        self.table.get(&h).map(|v| v.as_slice()).unwrap_or(&[])
    }
}

#[allow(clippy::too_many_arguments)]
fn find_covers(old: &mut CombinedStream, old_size: u64, new: &mut CombinedStream, new_size: u64) -> io::Result<(Vec<Cover>, Vec<u8>)> {
    let index = OldIndex::build(old, old_size)?;

    let mut covers = Vec::new();
    let mut new_data = Vec::new();
    let mut pending_gap = 0u64;

    let mut new_buf = vec![0u8; BLOCK];
    let mut old_buf = vec![0u8; BLOCK];
    let mut pos = 0u64;
    new.seek(SeekFrom::Start(0))?;

    while pos + BLOCK as u64 <= new_size {
        new.seek(SeekFrom::Start(pos))?;
        new.read_exact(&mut new_buf)?;
        let h = hash_block(&new_buf);

        let mut best: Option<(u64, u64)> = None;
        for &cand in index.candidates(h) {
            old.seek(SeekFrom::Start(cand))?;
            if old.read_exact(&mut old_buf).is_err() { continue; }
            if old_buf != new_buf[..] { continue; }

            // Verified block match; extend forward while bytes agree.
            let mut len = BLOCK as u64;
            let mut a = vec![0u8; BLOCK];
            let mut b = vec![0u8; BLOCK];
            loop {
                let remaining_old = old_size.saturating_sub(cand + len);
                let remaining_new = new_size.saturating_sub(pos + len);
                let step = BLOCK.min(remaining_old.min(remaining_new) as usize);
                if step == 0 { break; }
                old.seek(SeekFrom::Start(cand + len))?;
                old.read_exact(&mut a[..step])?;
                new.seek(SeekFrom::Start(pos + len))?;
                new.read_exact(&mut b[..step])?;
                let same = a[..step].iter().zip(&b[..step]).take_while(|(x, y)| x == y).count();
                len += same as u64;
                if same < step { break; }
            }
            if best.map(|(_, bl)| len > bl).unwrap_or(true) { best = Some((cand, len)); }
        }

        match best {
            Some((old_pos, len)) => {
                covers.push(Cover { old_pos, new_gap: pending_gap, length: len });
                pending_gap = 0;
                pos += len;
            }
            None => {
                new.seek(SeekFrom::Start(pos))?;
                let mut one = [0u8; 1];
                new.read_exact(&mut one)?;
                new_data.push(one[0]);
                pending_gap += 1;
                pos += 1;
            }
        }
    }

    // Whatever is left after the last match is emitted verbatim.
    if pos < new_size {
        new.seek(SeekFrom::Start(pos))?;
        let mut tail = vec![0u8; (new_size - pos) as usize];
        new.read_exact(&mut tail)?;
        new_data.extend_from_slice(&tail);
    }

    Ok((covers, new_data))
}

fn compress(data: &[u8], enabled: bool) -> io::Result<(Vec<u8>, bool)> {
    if !enabled || data.is_empty() { return Ok((data.to_vec(), false)); }
    let packed = zstd::stream::encode_all(data, 3)?;
    if packed.len() < data.len() { Ok((packed, true)) } else { Ok((data.to_vec(), false)) }
}

pub(crate) fn create(old_dir: &str, new_dir: &str, out_path: &str, opts: &KrCreateOptions) -> io::Result<()> {
    let old_root = PathBuf::from(old_dir);
    let new_root = PathBuf::from(new_dir);

    let (old_paths, old_files) = walk_tree(&old_root)?;
    let (new_paths, new_files) = walk_tree(&new_root)?;

    let old_sizes: Vec<u64> = old_files.iter().map(|f| f.size).collect();
    let new_sizes: Vec<u64> = new_files.iter().map(|f| f.size).collect();
    let old_ref_size: u64 = old_sizes.iter().sum();
    let new_ref_size: u64 = new_sizes.iter().sum();

    let old_off = crate::utils::patch::krdir::encode_path_offsets(&file_indices(&old_paths, &old_files));
    let new_off = crate::utils::patch::krdir::encode_path_offsets(&file_indices(&new_paths, &new_files));

    let mut digests = Vec::with_capacity(new_files.len());
    for f in &new_files {
        digests.push(digest_file(&new_root.join(&f.path))?);
    }

    let mut head = Vec::new();
    for p in &old_paths { head.extend_from_slice(p.as_bytes()); head.push(0); }
    for p in &new_paths { head.extend_from_slice(p.as_bytes()); head.push(0); }
    for v in &old_off { pack_uint(&mut head, *v); }
    for v in &new_off { pack_uint(&mut head, *v); }
    for v in &old_sizes { pack_uint(&mut head, *v); }
    for v in &new_sizes { pack_uint(&mut head, *v); }
    for v in &digests { pack_uint(&mut head, *v); }

    let old_path_sum: u64 = old_paths.iter().map(|p| p.len() as u64 + 1).sum();
    let new_path_sum: u64 = new_paths.iter().map(|p| p.len() as u64 + 1).sum();

    let (head_packed, head_compressed) = compress(&head, opts.compress)?;

    let mut old_combined = CombinedStream::new(open_all(&old_root, &old_files)?)?;
    let mut new_combined = CombinedStream::new(open_all(&new_root, &new_files)?)?;
    let (covers, new_data) = if old_ref_size > 0 && new_ref_size > 0 {
        find_covers(&mut old_combined, old_ref_size, &mut new_combined, new_ref_size)?
    } else {
        let mut buf = Vec::new();
        if new_ref_size > 0 {
            new_combined.seek(SeekFrom::Start(0))?;
            new_combined.read_to_end(&mut buf)?;
        }
        (Vec::new(), buf)
    };

    let mut cover_buf = Vec::new();
    let mut read_pos: i64 = 0;
    for c in &covers {
        pack_signed(&mut cover_buf, c.old_pos as i64 - read_pos);
        pack_uint(&mut cover_buf, c.new_gap);
        pack_uint(&mut cover_buf, c.length);
        read_pos = c.old_pos as i64 + c.length as i64;
    }

    let (cover_packed, cover_compressed) = compress(&cover_buf, opts.compress)?;
    let (data_packed, data_compressed) = compress(&new_data, opts.compress)?;

    let comp_name: &str = if opts.compress { "zstd" } else { "" };
    let mut out = io::BufWriter::new(File::create(out_path)?);

    out.write_all(b"HDIFF19&")?;
    out.write_all(comp_name.as_bytes())?;
    out.write_all(b"&fadler64\0")?;
    out.write_all(&[1u8, 1u8])?;

    let mut hdr = Vec::new();
    pack_uint(&mut hdr, old_paths.len() as u64);
    pack_uint(&mut hdr, old_path_sum);
    pack_uint(&mut hdr, new_paths.len() as u64);
    pack_uint(&mut hdr, new_path_sum);
    pack_uint(&mut hdr, old_files.len() as u64);
    pack_uint(&mut hdr, old_ref_size);
    pack_uint(&mut hdr, new_files.len() as u64);
    pack_uint(&mut hdr, new_ref_size);
    pack_uint(&mut hdr, 0); // same file pairs
    pack_uint(&mut hdr, 0); // same file size
    pack_uint(&mut hdr, 0); // new execute count
    pack_uint(&mut hdr, 0); // private reserved
    pack_uint(&mut hdr, 0); // private extern
    pack_uint(&mut hdr, 0); // extern
    pack_uint(&mut hdr, head.len() as u64);
    pack_uint(&mut hdr, if head_compressed { head_packed.len() as u64 } else { 0 });
    pack_uint(&mut hdr, 8); // checksum byte size
    out.write_all(&hdr)?;

    // oldRef, newRef, sameFile, diff. Verified against real patches: newRef is
    // the per-file digests folded with the fadler64 combine identity, and
    // sameFile is ADLER_INITIAL when there are no same-file pairs. The diff
    // digest covers the finished patch file and is left zero.
    let mut old_digests = Vec::with_capacity(old_files.len());
    for f in &old_files {
        old_digests.push(digest_file(&old_root.join(&f.path))?);
    }
    out.write_all(&fold_fadler64(&old_digests, &old_sizes).to_le_bytes())?;
    out.write_all(&fold_fadler64(&digests, &new_sizes).to_le_bytes())?;
    out.write_all(&ADLER_INITIAL.to_le_bytes())?;
    out.write_all(&0u64.to_le_bytes())?;
    out.write_all(&head_packed)?;

    out.write_all(b"HDIFF13&")?;
    out.write_all(comp_name.as_bytes())?;
    out.write_all(b"\0")?;

    let mut h13 = Vec::new();
    pack_uint(&mut h13, new_ref_size);
    pack_uint(&mut h13, old_ref_size);
    pack_uint(&mut h13, covers.len() as u64);
    pack_uint(&mut h13, cover_buf.len() as u64);
    pack_uint(&mut h13, if cover_compressed { cover_packed.len() as u64 } else { 0 });
    pack_uint(&mut h13, 0); // rle ctrl
    pack_uint(&mut h13, 0);
    pack_uint(&mut h13, 0); // rle code
    pack_uint(&mut h13, 0);
    pack_uint(&mut h13, new_data.len() as u64);
    pack_uint(&mut h13, if data_compressed { data_packed.len() as u64 } else { 0 });
    out.write_all(&h13)?;

    out.write_all(&cover_packed)?;
    out.write_all(&data_packed)?;
    out.flush()?;
    Ok(())
}

fn open_all(root: &Path, files: &[Entry]) -> io::Result<Vec<File>> {
    files.iter().map(|f| File::open(root.join(&f.path))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::binary::BinaryExtensions;
    use std::io::Cursor;

    fn decode_uint(bytes: &[u8]) -> u64 {
        let mut c = Cursor::new(bytes);
        c.read_long_7bit().unwrap() as u64
    }

    fn decode_signed(bytes: &[u8]) -> i64 {
        let mut c = Cursor::new(bytes);
        let mut first = [0u8; 1];
        std::io::Read::read_exact(&mut c, &mut first).unwrap();
        let neg = (first[0] >> 7) != 0;
        let mag = c.read_long_7bit_tagged(1, first[0]).unwrap();
        if neg { -mag } else { mag }
    }

    #[test]
    fn packed_uint_round_trips() {
        let mut values = vec![0u64, 1, 63, 64, 127, 128, 16383, 16384, u32::MAX as u64, u64::MAX >> 1];
        for bits in 0..63 { values.push(1u64 << bits); }
        for v in values {
            let mut buf = Vec::new();
            pack_uint(&mut buf, v);
            assert_eq!(decode_uint(&buf), v, "uint {}", v);
        }
    }

    #[test]
    fn packed_signed_round_trips() {
        let mut values = vec![0i64, 1, -1, 63, -63, 64, -64, 8191, -8191, 8192, -8192];
        for bits in 0..62 {
            values.push(1i64 << bits);
            values.push(-(1i64 << bits));
        }
        for v in values {
            let mut buf = Vec::new();
            pack_signed(&mut buf, v);
            assert_eq!(decode_signed(&buf), v, "signed {}", v);
        }
    }

    #[test]
    fn path_offsets_round_trip() {
        for indices in [vec![1u64, 2, 3], vec![0, 5, 9], vec![3], vec![2, 4, 6, 8]] {
            let offsets = crate::utils::patch::krdir::encode_path_offsets(&indices);
            let paths: Vec<String> = (0..12).map(|i| format!("p{}", i)).collect();
            let sizes = vec![0u64; indices.len()];
            let (files, _) = crate::utils::patch::krdir::split_paths_with_offsets(&paths, &offsets, &sizes);
            let got: Vec<u64> = files.iter().map(|f| paths.iter().position(|p| *p == f.path).unwrap() as u64).collect();
            assert_eq!(got, indices, "offsets {:?}", offsets);
        }
    }
}

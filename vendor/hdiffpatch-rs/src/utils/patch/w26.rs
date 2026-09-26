use std::io::{Error, ErrorKind, Read, Seek, SeekFrom, Write};

use crate::utils::checksum::Checksum;
use crate::utils::compression::{IO_BUF_SIZE, open_decompressor};
use crate::utils::mt::{PatchOptions, ThreadedReader, WindowPrefetcher};
use crate::utils::patch::step_engine::{MemOld, patch_step_loop, read_packed_uint, unpack_uint, read_sign_pos_by_last_pos};
use crate::utils::types::{ChecksumMode, CompressionMode, SeekableRead};

pub(crate) const W26_MAGIC: &[u8; 8] = b"HDIFFW26";
const HEAD_MAX_SIZE: usize = 4096;
const MAX_WINDOW_META_COUNT: u64 = 64;
const STEP_MEM_SIZE_SAFE_LIMIT: u64 = 4 << 20;

fn bad(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidData, msg.into())
}

#[derive(Debug, Clone, Default)]
pub(crate) struct WindowDiffInfo {
    pub compress_type: String,
    pub checksum_type: String,
    pub new_data_size: u64,
    pub old_data_size: u64,
    pub cover_count: u64,
    pub window_count: u64,
    pub window_meta_count: u64,
    pub max_step_mem_size: u64,
    pub max_sub_cover_count: u64,
    pub max_window_old_size: u64,
    pub checksum_byte_size: u64,
    pub extra_data_size: u64,
    pub uncompressed_size: u64,
    pub compressed_size: u64,
    pub other_info_pos: u64,
    pub other_info_end_pos: u64,
    pub window_data_pos: u64,
    pub head: Vec<u8>,
}

impl WindowDiffInfo {
    fn stored_checksum(&self, index: usize) -> &[u8] {
        let size = self.checksum_byte_size as usize;
        let base = self.window_data_pos as usize - 3 * size + index * size;
        &self.head[base..base + size]
    }

    pub(crate) fn stored_checksum_new(&self) -> &[u8] { self.stored_checksum(1) }

    #[allow(dead_code)]
    pub(crate) fn stored_checksum_old(&self) -> &[u8] { self.stored_checksum(0) }
    #[allow(dead_code)]
    pub(crate) fn stored_checksum_diff(&self) -> &[u8] { self.stored_checksum(2) }

    pub(crate) fn try_parse<R: Read + Seek>(sr: &mut R, diff_info_pos: u64) -> std::io::Result<Option<Self>> {
        const PREFIX: usize = 10; // magic + 2 little-endian length bytes

        sr.seek(SeekFrom::Start(diff_info_pos))?;
        let mut prefix = [0u8; PREFIX];
        if sr.read_exact(&mut prefix).is_err() { return Ok(None); }
        if &prefix[..8] != W26_MAGIC { return Ok(None); }

        let head_remaining = prefix[8] as usize | ((prefix[9] as usize) << 8);
        if head_remaining + PREFIX > HEAD_MAX_SIZE { return Err(bad("[HDIFFW26] head size exceeds the maximum")); }

        let mut head = vec![0u8; PREFIX + head_remaining];
        head[..PREFIX].copy_from_slice(&prefix);
        sr.read_exact(&mut head[PREFIX..])?;

        let mut info = WindowDiffInfo::default();
        info.window_data_pos = (PREFIX + head_remaining) as u64;

        let mut p: &[u8] = &head[PREFIX..];
        info.compress_type = read_type_end(&mut p, b'&')?;
        info.checksum_type = read_type_end(&mut p, 0)?;

        info.compressed_size = unpack_uint(&mut p)?;
        info.uncompressed_size = unpack_uint(&mut p)?;
        info.new_data_size = unpack_uint(&mut p)?;
        info.old_data_size = unpack_uint(&mut p)?;
        info.cover_count = unpack_uint(&mut p)?;
        info.window_count = unpack_uint(&mut p)?;
        info.window_meta_count = unpack_uint(&mut p)?;
        info.max_step_mem_size = unpack_uint(&mut p)?;
        info.max_sub_cover_count = unpack_uint(&mut p)?;
        info.max_window_old_size = unpack_uint(&mut p)?;
        info.checksum_byte_size = unpack_uint(&mut p)?;
        info.extra_data_size = unpack_uint(&mut p)?;

        // Whatever sits between here and the checksum block is reserved and skipped.
        info.other_info_pos = (head.len() - p.len()) as u64;
        if info.other_info_pos + info.checksum_byte_size * 3 > info.window_data_pos {
            return Err(bad("[HDIFFW26] head too small for the checksum block"));
        }
        info.other_info_end_pos = info.window_data_pos - info.checksum_byte_size * 3;

        info.validate()?;
        info.head = head;
        Ok(Some(info))
    }

    fn validate(&self) -> std::io::Result<()> {
        if self.window_meta_count > MAX_WINDOW_META_COUNT || self.window_meta_count < 2 { return Err(bad("[HDIFFW26] windowMetaCount out of range")); }
        if self.window_meta_count & (self.window_meta_count - 1) != 0 { return Err(bad("[HDIFFW26] windowMetaCount is not a power of two")); }
        let has_type = !self.checksum_type.is_empty();
        if has_type != (self.checksum_byte_size != 0) { return Err(bad("[HDIFFW26] checksum type and byte size disagree")); }
        if self.max_window_old_size > self.old_data_size { return Err(bad("[HDIFFW26] maxWindowOldSize exceeds oldDataSize")); }
        if self.max_window_old_size > 0 && self.window_count == 0 { return Err(bad("[HDIFFW26] windowCount is zero but windows carry old data")); }
        if self.compressed_size > self.uncompressed_size { return Err(bad("[HDIFFW26] compressedSize exceeds uncompressedSize")); }
        if self.max_step_mem_size > self.new_data_size + STEP_MEM_SIZE_SAFE_LIMIT || self.max_step_mem_size > self.uncompressed_size + STEP_MEM_SIZE_SAFE_LIMIT { return Err(bad("[HDIFFW26] maxStepMemSize is implausibly large")); }
        Ok(())
    }
}

fn read_type_end(p: &mut &[u8], end: u8) -> std::io::Result<String> {
    let pos = p.iter().position(|&b| b == end).ok_or_else(|| bad("[HDIFFW26] unterminated type string in head"))?;
    let s = std::str::from_utf8(&p[..pos]).map_err(|_| bad("[HDIFFW26] type string is not valid UTF-8"))?.to_string();
    *p = &p[pos + 1..];
    Ok(s)
}

#[derive(Clone, Copy, Default)]
struct WinInfo {
    old_pos: u64,
    len: u64,
}

fn compute_window_overlap(prev_pos: u64, prev_end: u64, curr_pos: u64, curr_end: u64) -> (u64, u64, u64) {
    let o_start = prev_pos.max(curr_pos);
    let o_end = prev_end.min(curr_end);
    if o_start >= o_end {
        (0, 0, 0)
    } else if curr_pos >= prev_pos && curr_end <= prev_end {
        (curr_end - curr_pos, curr_pos - prev_pos, 0)
    } else if curr_pos >= prev_pos && curr_pos < prev_end {
        (prev_end - curr_pos, curr_pos - prev_pos, 0)
    } else if curr_end > prev_pos && curr_end <= prev_end {
        (curr_end - prev_pos, 0, prev_pos - curr_pos)
    } else {
        (prev_end - prev_pos, 0, prev_pos - curr_pos)
    }
}

pub(crate) struct PatchW {
    info: WindowDiffInfo,
    checksum_mode: ChecksumMode,
    options: PatchOptions,
}

impl PatchW {
    pub(crate) fn new(info: WindowDiffInfo) -> Self {
        let checksum_mode: ChecksumMode = info.checksum_type.parse().unwrap_or_default();
        Self { info, checksum_mode, options: PatchOptions::default() }
    }

    pub(crate) fn with_options(mut self, options: PatchOptions) -> Self {
        self.options = options;
        self
    }

    pub(crate) fn patch(&self, diff_file: std::fs::File, diff_info_pos: u64, old: &mut dyn SeekableRead, old_size: u64, out: &mut dyn Write, comp_override: Option<CompressionMode>, old_path: Option<&std::path::Path>) -> std::io::Result<()> {
        let info = &self.info;
        if old_size != info.old_data_size { return Err(bad(format!("[HDIFFW26] old data size mismatch: header says {}, input is {}", info.old_data_size, old_size))); }

        let comp_mode: CompressionMode = if info.compress_type.is_empty() { comp_override.unwrap_or(CompressionMode::Nocomp) } else { info.compress_type.parse().unwrap_or(CompressionMode::Nocomp) };

        // Body layout: [extraData][window metadata + step data], optionally compressed as one stream.
        let body_pos = diff_info_pos + info.window_data_pos;
        let on_disk_len = if info.compressed_size > 0 { info.compressed_size } else { info.uncompressed_size };

        let mut file = diff_file;
        file.seek(SeekFrom::Start(body_pos))?;
        let bounded = std::io::BufReader::with_capacity(IO_BUF_SIZE, file.take(on_disk_len));
        let raw: Box<dyn Read + Send> = if info.compressed_size > 0 {
            open_decompressor(comp_mode, bounded, info.uncompressed_size)?
        } else {
            Box::new(bounded)
        };
        // Overlaps decompression with the old-data reads and output writes.
        let pipeline = self.options.pipeline(IO_BUF_SIZE);
        let mut diff: Box<dyn Read> = if pipeline.is_threaded() {
            Box::new(ThreadedReader::new(raw, pipeline))
        } else {
            raw
        };

        // Buffers are sized from the validated header and reused for the whole patch.
        let mut old_buf = vec![0u8; info.max_window_old_size as usize];
        let mut step_buf = vec![0u8; info.max_step_mem_size as usize];
        let mut io_buf = vec![0u8; IO_BUF_SIZE];

        // Skip extraData, which precedes the window stream.
        let mut skipped = 0u64;
        while skipped < info.extra_data_size {
            let take = (io_buf.len() as u64).min(info.extra_data_size - skipped) as usize;
            diff.read_exact(&mut io_buf[..take])?;
            skipped += take as u64;
        }

        let mut checksum_new = if self.checksum_mode.checksum_byte_size() == Some(info.checksum_byte_size as usize) { self.checksum_mode.new_checksum() } else { None };
        let mut prefetch = match old_path {
            Some(path) if self.options.threads > 1 => WindowPrefetcher::new(path, info.max_window_old_size as usize, self.options.memory_budget, (info.window_meta_count as usize) + 2),
            _ => None,
        };

        let written = self.run_windows(&mut diff, old, out, &mut old_buf, &mut step_buf, &mut io_buf, &mut checksum_new, &mut prefetch)?;
        if written != info.new_data_size { return Err(bad(format!("[HDIFFW26] produced {} bytes but header declares {}", written, info.new_data_size))); }

        if let Some(cs) = checksum_new {
            let got = cs.finish();
            if got != info.stored_checksum_new() { return Err(bad("[HDIFFW26] new data checksum mismatch")); }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn run_windows(&self, diff: &mut dyn Read, old: &mut dyn SeekableRead, out: &mut dyn Write, old_buf: &mut [u8], step_buf: &mut [u8], io_buf: &mut [u8], checksum_new: &mut Option<Checksum>, prefetch: &mut Option<WindowPrefetcher>) -> std::io::Result<u64> {
        let info = &self.info;
        let meta_count = info.window_meta_count;
        let mut win_infos = [WinInfo::default(); MAX_WINDOW_META_COUNT as usize];

        // Old-window positions are delta-coded across the whole diff, so this
        // running value persists across metadata batches.
        let mut last_old_pos_for_meta = 0u64;
        let mut loaded_meta_end = 0u64;
        // Extent of the slice currently sitting in `old_buf`, for overlap reuse.
        let mut last_old_pos = 0u64;
        let mut last_old_end = 0u64;
        let mut written = 0u64;

        for wi in 0..info.window_count {
            // Metadata arrives in batches: a full ring at the start, then a
            // half-ring refill every metaCount/2 windows.
            if (wi & ((meta_count >> 1) - 1)) == 0 && loaded_meta_end < info.window_count {
                let saved = if wi == 0 { meta_count } else { meta_count >> 1 };
                let write_idx = (loaded_meta_end & (meta_count - 1)) as usize;
                let batch = (info.window_count - loaded_meta_end).min(saved);
                self.read_meta_batch(diff, &mut last_old_pos_for_meta, &mut win_infos, write_idx, batch as usize)?;
                if let Some(p) = prefetch.as_ref() {
                    for k in 0..batch as usize {
                        let w = win_infos[write_idx + k];
                        p.submit(w.old_pos, w.len as usize);
                    }
                }
                loaded_meta_end += batch;
            }

            let sub_cover_count = read_packed_uint(diff)?;
            if sub_cover_count > info.max_sub_cover_count {
                return Err(bad("[HDIFFW26] window sub-cover count exceeds maxSubCoverCount"));
            }

            let meta_idx = (wi & (meta_count - 1)) as usize;
            let window_old_pos = win_infos[meta_idx].old_pos;
            let window_old_len = win_infos[meta_idx].len as usize;

            // Cover state is per-window: the reference re-initialises it on
            // every `_patch_single_stream_loop` call, so positions are relative
            // to this window's old slice and to its slice of the new data.
            match prefetch.as_mut() {
                Some(p) => {
                    let buf = p.next()?;
                    run_window(diff, out, &buf[..window_old_len], sub_cover_count, step_buf, io_buf, &mut written, checksum_new)?;
                    p.recycle(buf);
                }
                None => {
                    self.load_window_old(old, old_buf, window_old_pos, window_old_len as u64, &mut last_old_pos, &mut last_old_end)?;
                    run_window(diff, out, &old_buf[..window_old_len], sub_cover_count, step_buf, io_buf, &mut written, checksum_new)?;
                }
            }
        }
        Ok(written)
    }

    fn read_meta_batch(&self, diff: &mut dyn Read, last_old_pos: &mut u64, win_infos: &mut [WinInfo], write_idx: usize, batch: usize) -> std::io::Result<()> {
        let info = &self.info;
        // The metadata block is packed varints; read it into a scratch buffer so
        // the slice-based unpacker can run over it.
        let mut scratch = Vec::with_capacity(batch * 24);
        for i in 0..batch {
            let len = read_packed_uint(diff)?;
            scratch.clear();
            read_signed_pos(diff, &mut scratch, last_old_pos)?;

            if len > info.max_window_old_size { return Err(bad("[HDIFFW26] window old length exceeds maxWindowOldSize")); }
            if *last_old_pos > info.old_data_size || len > info.old_data_size - *last_old_pos {
                return Err(bad("[HDIFFW26] window old range falls outside the old file"));
            }
            win_infos[write_idx + i] = WinInfo { old_pos: *last_old_pos, len };
            *last_old_pos += len;
        }
        Ok(())
    }

    fn load_window_old(&self, old: &mut dyn SeekableRead, old_buf: &mut [u8], window_old_pos: u64, window_old_len: u64, last_old_pos: &mut u64, last_old_end: &mut u64) -> std::io::Result<()> {
        let window_old_end = window_old_pos + window_old_len;
        let (reuse_len, reuse_off, no_overlap_left) = compute_window_overlap(*last_old_pos, *last_old_end, window_old_pos, window_old_end);

        if reuse_len > 0 {
            old_buf.copy_within(reuse_off as usize..(reuse_off + reuse_len) as usize, no_overlap_left as usize);
        }
        if no_overlap_left > 0 {
            old.seek(SeekFrom::Start(window_old_pos))?;
            old.read_exact(&mut old_buf[..no_overlap_left as usize])?;
        }
        if no_overlap_left + reuse_len < window_old_len {
            let from = no_overlap_left + reuse_len;
            old.seek(SeekFrom::Start(window_old_pos + from))?;
            old.read_exact(&mut old_buf[from as usize..window_old_len as usize])?;
        }

        *last_old_pos = window_old_pos;
        *last_old_end = window_old_end;
        Ok(())
    }
}

fn read_signed_pos(diff: &mut dyn Read, scratch: &mut Vec<u8>, last_pos: &mut u64) -> std::io::Result<()> {
    // The value is self-delimiting: read bytes until the continuation bit clears.
    let mut b = [0u8; 1];
    diff.read_exact(&mut b)?;
    scratch.push(b[0]);
    // Tag bit 1 means the continuation flag is bit 6 on the first byte.
    if (b[0] & 0x40) != 0 {
        loop {
            diff.read_exact(&mut b)?;
            scratch.push(b[0]);
            if (b[0] & 0x80) == 0 { break; }
        }
    }
    let mut p: &[u8] = scratch;
    read_sign_pos_by_last_pos(&mut p, last_pos)
}

#[allow(clippy::too_many_arguments)]
fn run_window(diff: &mut dyn Read, out: &mut dyn Write, window: &[u8], sub_cover_count: u64, step_buf: &mut [u8], io_buf: &mut [u8], written: &mut u64, checksum_new: &mut Option<Checksum>) -> std::io::Result<()> {
    let mut mem_old = MemOld { data: window };
    match checksum_new.as_mut() {
        Some(cs) => {
            let mut tee = ChecksumWriter { inner: out, checksum: cs };
            patch_step_loop(diff, &mut tee, &mut mem_old, sub_cover_count, step_buf, io_buf, written)
        }
        None => patch_step_loop(diff, out, &mut mem_old, sub_cover_count, step_buf, io_buf, written),
    }
}

struct ChecksumWriter<'a, 'b> {
    inner: &'a mut dyn Write,
    checksum: &'b mut Checksum,
}

impl<'a, 'b> Write for ChecksumWriter<'a, 'b> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write_all(buf)?;
        self.checksum.append(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> { self.inner.flush() }
}

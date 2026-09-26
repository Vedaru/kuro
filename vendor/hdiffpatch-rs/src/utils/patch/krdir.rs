use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::str::FromStr;

use crate::utils::binary::BinaryExtensions;
use crate::utils::compression::{IO_BUF_SIZE, get_clip_stream, open_decompressor_local};
use crate::utils::mt::{PatchOptions, ThreadedReader};
use crate::utils::checksum::Checksum;
use crate::utils::types::{ChecksumMode, CombinedStream, CompressionMode, NewFileCombinedStream};

pub(crate) struct KrFileEntry {
    pub path: String,
    pub size: u64,
}

pub(crate) struct KrHead {
    pub old_files: Vec<KrFileEntry>,
    pub new_files: Vec<KrFileEntry>,
    pub new_directories: Vec<String>,
    pub new_file_checksums: Vec<u64>,
}

pub(crate) struct KrHd19 {
    pub comp_mode: CompressionMode,
    pub checksum_type: String,
    pub old_ref_size: u64,
    pub new_ref_size: u64,
    pub head: KrHead,
}

pub(crate) struct KrCover {
    pub old_pos_delta: i64,
    pub new_pos_gap: u64,
    pub length: u64,
}

pub(crate) struct KrHd13 {
    pub covers: Vec<KrCover>,
    pub new_data_size: u64,
    pub new_data_diff_offset: u64,
    pub new_data_diff_size: u64,
    pub new_data_diff_comp_size: u64,
    pub comp_mode: CompressionMode,
}

fn read_section<R, T>(reader: &mut R, comp_mode: &CompressionMode, raw_size: u64, comp_size: u64, parse: impl FnOnce(&mut dyn Read) -> io::Result<T>) -> io::Result<T> where R: Read + Seek,{
    let section_start = reader.stream_position()?;
    let file_bytes = if comp_size > 0 { comp_size } else { raw_size };

    let value = {
        let limited = reader.by_ref().take(file_bytes);
        if comp_size > 0 {
            let mut dec = open_decompressor_local(comp_mode.clone(), limited, raw_size)?;
            parse(&mut *dec)?
        } else {
            let mut limited = limited;
            parse(&mut limited)?
        }
    };
    reader.seek(SeekFrom::Start(section_start + file_bytes))?;
    Ok(value)
}

pub(crate) fn parse_hd19<R: Read + Seek>(reader: &mut R) -> io::Result<KrHd19> {
    let chunk_type = read_delim(reader, b'&', 10)?;
    if chunk_type != "HDIFF19" { return Err(io::Error::new(io::ErrorKind::InvalidData, format!("[KrDiff] Expected HDIFF19 chunk, got {:?}", chunk_type))); }
    let comp_str = read_delim(reader, b'&', 10)?;
    let checksum_type = read_delim(reader, b'\0', 15)?;
    let _old_is_dir = reader.read_boolean()?;
    let _new_is_dir = reader.read_boolean()?;

    let old_path_count = reader.read_long_7bit()? as u64;
    let _old_path_sum_size = reader.read_long_7bit()?;
    let new_path_count = reader.read_long_7bit()? as u64;
    let _new_path_sum_size = reader.read_long_7bit()?;
    let old_ref_file_count = reader.read_long_7bit()? as u64;
    let old_ref_size = reader.read_long_7bit()? as u64;
    let new_ref_file_count = reader.read_long_7bit()? as u64;
    let new_ref_size = reader.read_long_7bit()? as u64;
    let _same_file_pair_count = reader.read_long_7bit()?;
    let _same_file_size = reader.read_long_7bit()?;
    let _new_execute_count = reader.read_long_7bit()?;
    let _private_reserved = reader.read_long_7bit()?;
    let private_extern_size = reader.read_long_7bit()? as u64;
    let extern_size = reader.read_long_7bit()? as u64;
    let head_data_size = reader.read_long_7bit()? as u64;
    let head_data_comp_size = reader.read_long_7bit()? as u64;
    let checksum_byte_size = reader.read_long_7bit()? as u64;

    skip_bytes(reader, checksum_byte_size * 4)?;

    let comp_mode = CompressionMode::from_str(&comp_str).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let head = read_section(reader, &comp_mode, head_data_size, head_data_comp_size, |r| { parse_head_data_seq(r, old_path_count, new_path_count, old_ref_file_count, new_ref_file_count) })?;

    skip_bytes(reader, private_extern_size)?;
    skip_bytes(reader, extern_size)?;
    Ok(KrHd19 { comp_mode, checksum_type, old_ref_size, new_ref_size, head })
}

fn parse_head_data_seq(reader: &mut dyn Read, old_path_count: u64, new_path_count: u64, old_ref_file_count: u64, new_ref_file_count: u64) -> io::Result<KrHead> {
    let mut reader = reader;

    let mut old_paths = Vec::with_capacity(old_path_count as usize);
    for _ in 0..old_path_count { old_paths.push(read_null_str(&mut reader)?); }

    let mut new_paths = Vec::with_capacity(new_path_count as usize);
    for _ in 0..new_path_count { new_paths.push(read_null_str(&mut reader)?); }

    let mut old_offsets = Vec::with_capacity(old_ref_file_count as usize);
    for _ in 0..old_ref_file_count { old_offsets.push(reader.read_long_7bit()? as u64); }
    let mut new_offsets = Vec::with_capacity(new_ref_file_count as usize);
    for _ in 0..new_ref_file_count { new_offsets.push(reader.read_long_7bit()? as u64); }

    let mut old_sizes = Vec::with_capacity(old_ref_file_count as usize);
    for _ in 0..old_ref_file_count { old_sizes.push(reader.read_long_7bit()? as u64); }

    let mut new_sizes = Vec::with_capacity(new_ref_file_count as usize);
    for _ in 0..new_ref_file_count { new_sizes.push(reader.read_long_7bit()? as u64); }

    let mut new_file_checksums = Vec::with_capacity(new_ref_file_count as usize);
    for _ in 0..new_ref_file_count { new_file_checksums.push(reader.read_long_7bit()? as u64); }

    let (old_files, _old_dirs) = split_paths_with_offsets(&old_paths, &old_offsets, &old_sizes);
    let (new_files, new_directories) = split_paths_with_offsets(&new_paths, &new_offsets, &new_sizes);
    Ok(KrHead { old_files, new_files, new_directories, new_file_checksums })
}

pub(crate) fn split_paths_with_offsets(paths: &[String], offsets: &[u64], sizes: &[u64]) -> (Vec<KrFileEntry>, Vec<String>) {
    let mut files = Vec::new();
    let mut dirs = Vec::new();

    if offsets.is_empty() {
        for path in paths { dirs.push(path.clone()); }
        return (files, dirs);
    }

    let mut offset_index: usize = 0;
    let mut next_file_index: u64 = offsets[0];

    for (i, path) in paths.iter().enumerate() {
        if i as u64 == next_file_index {
            if offset_index < offsets.len() - 1 {
                offset_index += 1;
                next_file_index += offsets[offset_index] + 1;
            }
            let size = sizes.get(files.len()).copied().unwrap_or(0);
            files.push(KrFileEntry { path: path.clone(), size });
        } else {
            dirs.push(path.clone());
        }
    }
    (files, dirs)
}

pub(crate) fn encode_path_offsets(file_indices: &[u64]) -> Vec<u64> {
    let mut offsets = Vec::with_capacity(file_indices.len());
    for (k, &idx) in file_indices.iter().enumerate() {
        if k == 0 { offsets.push(idx); } else { offsets.push(idx - file_indices[k - 1] - 1); }
    }
    offsets
}

pub(crate) fn parse_hd13<R: Read + Seek>(reader: &mut R) -> io::Result<KrHd13> {
    let chunk_type = read_delim(reader, b'&', 10)?;
    if chunk_type != "HDIFF13" { return Err(io::Error::new(io::ErrorKind::InvalidData, format!("[KrDiff] Expected HDIFF13 chunk, got {:?}", chunk_type))); }
    let comp_str = read_delim(reader, b'\0', 10)?;

    let new_data_size = reader.read_long_7bit()? as u64;
    let _old_data_size = reader.read_long_7bit()? as u64;
    let cover_count = reader.read_long_7bit()? as u64;
    let cover_buf_size = reader.read_long_7bit()? as u64;
    let comp_cover_buf_size = reader.read_long_7bit()? as u64;
    let rle_ctrl_buf_size = reader.read_long_7bit()? as u64;
    let comp_rle_ctrl_buf_size = reader.read_long_7bit()? as u64;
    let rle_code_buf_size = reader.read_long_7bit()? as u64;
    let comp_rle_code_buf_size = reader.read_long_7bit()? as u64;
    let new_data_diff_size = reader.read_long_7bit()? as u64;
    let new_data_diff_comp_size = reader.read_long_7bit()? as u64;

    let comp_mode = CompressionMode::from_str(&comp_str).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let cover_buf_start = reader.stream_position()?;
    let covers = read_section(reader, &comp_mode, cover_buf_size, comp_cover_buf_size, |r| { parse_covers_seq(r, cover_count) })?;

    let cover_file_bytes = if comp_cover_buf_size > 0 { comp_cover_buf_size } else { cover_buf_size };
    let rle_ctrl_file_bytes = if comp_rle_ctrl_buf_size > 0 { comp_rle_ctrl_buf_size } else { rle_ctrl_buf_size };
    let rle_code_file_bytes = if comp_rle_code_buf_size > 0 { comp_rle_code_buf_size } else { rle_code_buf_size };
    let new_data_diff_offset = cover_buf_start + cover_file_bytes + rle_ctrl_file_bytes + rle_code_file_bytes;

    Ok(KrHd13 {
        covers,
        new_data_size,
        new_data_diff_offset,
        new_data_diff_size,
        new_data_diff_comp_size,
        comp_mode,
    })
}

fn parse_covers_seq(reader: &mut dyn Read, cover_count: u64) -> io::Result<Vec<KrCover>> {
    let mut reader = reader;
    let mut covers = Vec::with_capacity(cover_count as usize);
    for _ in 0..cover_count {
        let mut first = [0u8; 1];
        reader.read_exact(&mut first)?;
        let p_sign = first[0];
        let sign = (p_sign >> 7) != 0;
        let abs_val = reader.read_long_7bit_tagged(1, p_sign)?;
        let old_pos_delta = if sign { -abs_val } else { abs_val };
        let new_pos_gap = reader.read_long_7bit()? as u64;
        let length = reader.read_long_7bit()? as u64;
        covers.push(KrCover { old_pos_delta, new_pos_gap, length });
    }
    Ok(covers)
}

fn read_delim(reader: &mut impl Read, delim: u8, limit: usize) -> io::Result<String> {
    let mut buf = Vec::with_capacity(16);
    let mut byte = [0u8; 1];
    loop {
        reader.read_exact(&mut byte)?;
        if byte[0] == delim { break; }
        buf.push(byte[0]);
        if buf.len() >= limit { break; }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn read_null_str(reader: &mut impl Read) -> io::Result<String> {
    read_delim(reader, b'\0', 255)
}

fn skip_bytes<R: Seek>(reader: &mut R, n: u64) -> io::Result<()> {
    if n > 0 { reader.seek(SeekFrom::Current(n as i64))?; }
    Ok(())
}

pub(crate) struct KrPatchDir {
    patch_path: String,
    options: PatchOptions,
}

impl KrPatchDir {
    pub fn new(patch_path: String) -> Self {
        Self { patch_path, options: PatchOptions::default() }
    }

    pub fn with_options(mut self, options: PatchOptions) -> Self {
        self.options = options;
        self
    }

    pub fn patch(&self, input: &str, output: &str) -> io::Result<()> {
        let base_input = PathBuf::from(input);
        let base_output = PathBuf::from(output);

        let mut f = File::open(&self.patch_path)?;
        let hd19 = parse_hd19(&mut f)?;
        let hd13 = parse_hd13(&mut f)?;

        for dir in &hd19.head.new_directories {
            if !dir.is_empty() { fs::create_dir_all(base_output.join(dir.trim_end_matches('/')))?; }
        }

        for fe in &hd19.head.old_files {
            let full = base_input.join(&fe.path);
            if !full.exists() { return Err(io::Error::new(io::ErrorKind::NotFound, format!("[KrDiff] Old file not found: {}", full.display()))); }
            let actual = full.metadata()?.len();
            if actual != fe.size { return Err(io::Error::new(io::ErrorKind::InvalidData, format!("[KrDiff] Old file size mismatch for {}: expected {} bytes, got {}", full.display(), fe.size, actual))); }
        }

        for fe in &hd19.head.new_files {
            let full = base_output.join(&fe.path);
            if let Some(parent) = full.parent() { fs::create_dir_all(parent)?; }
            let file = File::options().read(true).write(true).create(true).truncate(true).open(&full)?;
            file.set_len(fe.size)?;
        }

        if hd19.head.old_files.is_empty() || hd19.head.new_files.is_empty() { return Ok(()); }

        let old_handles: Vec<File> = hd19.head.old_files.iter().map(|fe| File::open(base_input.join(&fe.path))).collect::<io::Result<_>>()?;
        let mut old_combined = CombinedStream::new(old_handles)?;

        let new_handles: Vec<NewFileCombinedStream> = hd19.head.new_files.iter().map(|fe| {
            let full = base_output.join(&fe.path);
            let file = File::options().read(true).write(true).open(&full)?;
            Ok(NewFileCombinedStream { file, size: fe.size })
        }).collect::<io::Result<_>>()?;
        let mut new_combined = CombinedStream::from_new_files(new_handles)?;

        let sizes: Vec<u64> = hd19.head.new_files.iter().map(|f| f.size).collect();
        let mut sink = DigestSink::new(&mut new_combined, &sizes);
        self.apply(&hd13, hd19.old_ref_size, hd19.new_ref_size, &mut old_combined, &mut sink)?;
        let digests = sink.finish();
        new_combined.flush()?;

        verify_new_files(&hd19.head, &digests)?;
        Ok(())
    }

    fn apply(&self, hd13: &KrHd13, old_ref_size: u64, new_ref_size: u64, old_combined: &mut CombinedStream, new_combined: &mut dyn Write) -> io::Result<()> {
        let f_newdata = File::open(&self.patch_path)?;
        let (new_data, _) = get_clip_stream(f_newdata, hd13.comp_mode.clone(), hd13.new_data_diff_offset, hd13.new_data_diff_size, hd13.new_data_diff_comp_size, false)?;

        let pipeline = self.options.pipeline(IO_BUF_SIZE);
        let mut new_data: Box<dyn Read> = if pipeline.is_threaded() { Box::new(ThreadedReader::new(new_data, pipeline)) } else { new_data };

        let mut read_pos: i64 = 0;
        let mut write_pos: u64 = 0;
        let mut buf = vec![0u8; IO_BUF_SIZE];

        for cover in &hd13.covers {
            read_pos = read_pos.wrapping_add(cover.old_pos_delta);

            // The read position is cumulative and wraps, unlike vanilla HDiff
            // where each cover carries an absolute delta from the last cover end.
            if old_ref_size > 0 {
                let sz = old_ref_size as i64;
                while read_pos > sz { read_pos -= sz; }
                while read_pos < 0 { read_pos += sz; }
            }

            if cover.new_pos_gap > 0 {
                copy_n(&mut *new_data, new_combined, cover.new_pos_gap, &mut buf)?;
                write_pos += cover.new_pos_gap;
            }

            if cover.length > 0 {
                old_combined.seek(SeekFrom::Start(read_pos as u64))?;
                copy_n(old_combined, new_combined, cover.length, &mut buf)?;
            }

            read_pos = read_pos.wrapping_add(cover.length as i64);
            write_pos = write_pos.saturating_add(cover.length);
        }
        if write_pos < new_ref_size { copy_n(&mut *new_data, new_combined, new_ref_size - write_pos, &mut buf)?; }
        Ok(())
    }
}

fn copy_n(src: &mut dyn Read, dst: &mut dyn Write, mut n: u64, buf: &mut [u8]) -> io::Result<()> {
    while n > 0 {
        let to_read = (buf.len() as u64).min(n) as usize;
        src.read_exact(&mut buf[..to_read])?;
        dst.write_all(&buf[..to_read])?;
        n -= to_read as u64;
    }
    Ok(())
}

struct DigestSink<'a> {
    inner: &'a mut CombinedStream,
    sizes: &'a [u64],
    idx: usize,
    remaining: u64,
    cur: Option<Checksum>,
    done: Vec<u64>,
}

impl<'a> DigestSink<'a> {
    fn new(inner: &'a mut CombinedStream, sizes: &'a [u64]) -> Self {
        let mut me = Self { inner, sizes, idx: 0, remaining: 0, cur: None, done: Vec::new() };
        me.start();
        me
    }

    fn start(&mut self) {
        while self.idx < self.sizes.len() {
            self.remaining = self.sizes[self.idx];
            self.cur = ChecksumMode::Fadler64.new_checksum();
            if self.remaining > 0 {
                return;
            }
            // A zero-length file closes immediately.
            self.done.push(finish_fadler64(self.cur.take()));
            self.idx += 1;
        }
        self.cur = None;
    }

    fn finish(mut self) -> Vec<u64> {
        if self.cur.is_some() && self.remaining == 0 {
            self.done.push(finish_fadler64(self.cur.take()));
        }
        self.done
    }
}

impl<'a> Write for DigestSink<'a> {
    fn write(&mut self, mut data: &[u8]) -> io::Result<usize> {
        let total = data.len();
        while !data.is_empty() {
            if self.cur.is_none() {
                self.inner.write_all(data)?;
                break;
            }
            let take = (self.remaining as usize).min(data.len());
            self.inner.write_all(&data[..take])?;
            if let Some(c) = self.cur.as_mut() { c.append(&data[..take]); }
            self.remaining -= take as u64;
            data = &data[take..];
            if self.remaining == 0 {
                self.done.push(finish_fadler64(self.cur.take()));
                self.idx += 1;
                self.start();
            }
        }
        Ok(total)
    }

    fn flush(&mut self) -> io::Result<()> { self.inner.flush() }
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

fn verify_new_files(head: &KrHead, got: &[u64]) -> io::Result<()> {
    if head.new_file_checksums.len() != head.new_files.len() || got.len() != head.new_files.len() {
        return Ok(());
    }
    for (i, expected) in head.new_file_checksums.iter().enumerate() {
        if got[i] != *expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("[KrDiff] Checksum mismatch for {}", head.new_files[i].path),
            ));
        }
    }
    Ok(())
}

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::utils::checksum::Checksum;
use crate::utils::mt::{PatchOptions, ThreadedReader};
use crate::utils::compression::{IO_BUF_SIZE, get_clip_stream};
use crate::utils::header::Header;
use crate::utils::binary::BinaryExtensions;
use crate::utils::patch::sf20::PatchSF;
use crate::utils::patch::w26::PatchW;
use crate::utils::types::PatchCoreImpl;
use crate::utils::types::{CombinedStream, DataReferenceInfo, DirectoryReferencePair, HeaderInfo, NewFileCombinedStream, PatchCore};

pub(crate) struct PatchDir {
    options: PatchOptions,
    header_info: HeaderInfo,
    reference_info: DataReferenceInfo,
    patch_path: String,
}

impl PatchDir {
    pub fn new(header_info: HeaderInfo, reference_info: DataReferenceInfo, patch_path: String) -> Self {
        Self { options: PatchOptions::default(), header_info, reference_info, patch_path }
    }

    pub fn set_options(&mut self, options: PatchOptions) {
        self.options = options;
    }

    pub fn patch(&mut self, input: &str, output: &str) -> std::io::Result<()> {
        let base_input  = PathBuf::from(input);
        let base_output = PathBuf::from(output);
        let padding: u64 = 0;

        let ri = &self.reference_info;
        let header_padding  = if ri.head_data_compressed_size > 0 { padding } else { 0 };
        let head_comp_size  = (ri.head_data_compressed_size as u64).saturating_sub(header_padding);

        let head_file = File::open(&self.patch_path)?;
        let (mut head_stream, _) = get_clip_stream(head_file, self.header_info.comp_mode.clone(), ri.head_data_offset as u64 + header_padding, ri.head_data_size as u64, head_comp_size, true)?;
        let dir_data = self.init_dir_patcher(&mut *head_stream)?;

        let old_files = Self::get_ref_old_streams(&dir_data, &base_input)?;
        let new_files = Self::get_ref_new_streams(&dir_data, &base_output)?;

        let mut patch_for_inner = File::open(&self.patch_path)?;
        let mut inner_ref = DataReferenceInfo::default();
        Header::try_parse_header_info(&mut patch_for_inner, &self.patch_path, &mut self.header_info, &mut inner_ref).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

        let padding: u64 = 0;
        let mut old_combined = CombinedStream::new(old_files)?;
        let mut new_combined = CombinedStream::from_new_files(new_files)?;

        let old_len = old_combined.length();
        if old_len as i64 != self.header_info.old_data_size { return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("[PatchDir::patch] Old size mismatch: expected {} bytes, got {} bytes", self.header_info.old_data_size, old_combined.length()))); }

        // An embedded HDIFFSF20/HDIFFW26 payload carries no checksum of its own
        // (its type string is empty); integrity for a directory diff is the
        // HDIFF19-level newRef digest, so it is verified here for every payload.
        let mut new_checksum = self.new_ref_checksum();

        // The payload inside a directory diff is itself one of the three diff
        // formats; dispatch on whichever the inner header identified.
        let is_window = self.header_info.window_diff_info.is_some();
        if is_window || self.header_info.is_single_compressed_diff {
            let core = PatchCoreImpl::new(base_input, base_output);
            core.prepare_dir_outputs(&dir_data);

            let mut sink = ChecksumSink { inner: &mut new_combined, checksum: new_checksum.as_mut().map(|(c, _)| c) };
            if let Some(info) = self.header_info.window_diff_info.clone() {
                let diff_file = File::open(&self.patch_path)?;
                PatchW::new(info).with_options(self.options).patch(diff_file, inner_ref.hdiff_data_offset as u64, &mut old_combined, old_len, &mut sink, Some(self.header_info.comp_mode.clone()), None)?;
            } else {
                // diff_data_pos is stored relative to the payload; make it absolute.
                let mut hi = self.header_info.clone();
                hi.single_chunk_info.diff_data_pos += inner_ref.hdiff_data_offset;
                PatchSF::new(hi).with_options(self.options).patch(&mut old_combined, &mut sink, &self.patch_path)?;
            }
            new_combined.flush()?;
            return Self::verify_new_ref(new_checksum);
        }

        let mut core = PatchCoreImpl::new(base_input, base_output);
        core.set_directory_reference_pair(dir_data);
        self.start_patch_routine(&mut old_combined, &mut new_combined, &mut core, padding, new_checksum.as_mut().map(|(c, _)| c))?;
        new_combined.flush()?;
        Self::verify_new_ref(new_checksum)
    }

    fn new_ref_checksum(&self) -> Option<(Checksum, Vec<u8>)> {
        let size = self.reference_info.checksum_byte_size as usize;
        if size == 0 || self.reference_info.checksums.len() < size * 2 { return None; }
        if self.header_info.checksum_mode.checksum_byte_size() != Some(size) { return None; }
        let checksum = self.header_info.checksum_mode.new_checksum()?;
        Some((checksum, self.reference_info.checksums[size..size * 2].to_vec()))
    }

    fn verify_new_ref(state: Option<(Checksum, Vec<u8>)>) -> std::io::Result<()> {
        if let Some((checksum, expected)) = state {
            if checksum.finish() != expected { return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "[PatchDir::patch] New data checksum mismatch")); }
        }
        Ok(())
    }

    fn start_patch_routine(&self, old_stream: &mut CombinedStream, new_stream: &mut CombinedStream, core: &mut PatchCoreImpl, padding: u64, checksum: Option<&mut Checksum>) -> std::io::Result<()> {
        let hi = &self.header_info;
        let ci = &hi.chunk_info;

        let f0 = File::open(&self.patch_path)?;
        let f1 = File::open(&self.patch_path)?;
        let f2 = File::open(&self.patch_path)?;
        let f3 = File::open(&self.patch_path)?;

        // head_end_pos is the absolute offset in the patch file where the clips begin.
        let mut offset = ci.head_end_pos as u64;

        // clip[0]: cover_buf (always buffered in memory)
        let cover_padding = if ci.compress_cover_buf_size > 0 { padding } else { 0 };
        let (clip0, len0) = get_clip_stream(f0, hi.comp_mode.clone(), offset + cover_padding, ci.cover_buf_size as u64, ci.compress_cover_buf_size as u64, true)?;
        offset += len0;

        // clip[1]: rle_ctrl_buf (buffered)
        let rle_ctrl_padding = if ci.compress_rle_ctrl_buf_size > 0 { padding } else { 0 };
        let (clip1, len1) = get_clip_stream(f1, hi.comp_mode.clone(), offset + rle_ctrl_padding, ci.rle_ctrl_buf_size as u64, ci.compress_rle_ctrl_buf_size as u64, true)?;
        offset += len1;

        // clip[2]: rle_code_buf (buffered)
        let rle_code_padding = if ci.compress_rle_code_buf_size > 0 { padding } else { 0 };
        let (clip2, len2) = get_clip_stream(f2, hi.comp_mode.clone(), offset + rle_code_padding, ci.rle_code_buf_size as u64, ci.compress_rle_code_buf_size as u64, true)?;
        offset += len2;

        // clip[3]: new_data_diff (lazy — can be very large)
        let new_data_diff_padding = if ci.compress_new_data_diff_size > 0 { padding } else { 0 };
        let comp_diff_size = (ci.compress_new_data_diff_size as u64).saturating_sub(padding);
        let (clip3, _) = get_clip_stream(f3, hi.comp_mode.clone(), offset + new_data_diff_padding, ci.new_data_diff_size as u64, comp_diff_size, false)?;
        let pipeline = self.options.pipeline(IO_BUF_SIZE);
        let clip3: Box<dyn Read + Send> = if pipeline.is_threaded() { Box::new(ThreadedReader::new(clip3, pipeline)) } else { clip3 };
        let mut clips: [Box<dyn Read + Send>; 4] = [clip0, clip1, clip2, clip3];
        let mut sink = ChecksumSink { inner: new_stream, checksum };
        core.uncover_buffer_clips_stream(&mut clips, old_stream, &mut sink, hi);
        Ok(())
    }

    fn init_dir_patcher(&self, mut reader: &mut dyn Read) -> std::io::Result<DirectoryReferencePair> {
        let ri = &self.reference_info;
        // Old and new path lists (null-separated strings packed into a fixed-size buffer).
        let old_utf8_path_list = reader.get_paths_from_stream(ri.input_sum_size as usize, ri.input_dir_count as usize)?;
        let new_utf8_path_list = reader.get_paths_from_stream(ri.output_sum_size as usize, ri.output_dir_count as usize)?;
        // Reference index lists (delta-encoded, validated against path count).
        let old_ref_list = reader.get_longs_from_stream(ri.input_ref_file_count as usize, Some(ri.input_dir_count))?;
        let new_ref_list = reader.get_longs_from_stream(ri.output_ref_file_count as usize, Some(ri.output_dir_count))?;
        // New-file sizes (raw absolute values, not delta-encoded).
        let new_ref_size_list = reader.get_longs_from_stream_absolute(ri.output_ref_file_count as usize)?;
        // Same-file pairs (new-old index pairs, delta-encoded with sign bit).
        let data_same_pair_list = reader.get_pair_index_reference_from_stream(ri.same_file_pair_count as usize, ri.output_dir_count, ri.input_dir_count)?;
        // New-execute list (delta-encoded indices into new path list).
        let new_execute_list = reader.get_longs_from_stream(ri.new_execute_count as usize, Some(ri.output_dir_count))?;
        Ok(DirectoryReferencePair {
            old_utf8_path_list,
            new_utf8_path_list,
            old_ref_list,
            new_ref_list,
            new_ref_size_list,
            data_same_pair_list,
            new_execute_list,
        })
    }

    fn get_ref_old_streams(dir_data: &DirectoryReferencePair, base_input: &Path) -> std::io::Result<Vec<File>> {
        let mut streams = Vec::with_capacity(dir_data.old_ref_list.len());
        for &ref_idx in &dir_data.old_ref_list {
            let path     = &dir_data.old_utf8_path_list[ref_idx as usize];
            let full_path = base_input.join(path);
            streams.push(File::open(&full_path)?);
        }
        Ok(streams)
    }

    fn get_ref_new_streams(dir_data: &DirectoryReferencePair, base_output: &Path) -> std::io::Result<Vec<NewFileCombinedStream>> {
        let mut streams = Vec::with_capacity(dir_data.new_ref_list.len());
        for (i, &ref_idx) in dir_data.new_ref_list.iter().enumerate() {
            let path      = &dir_data.new_utf8_path_list[ref_idx as usize];
            let full_path  = base_output.join(path);
            if let Some(parent) = full_path.parent() { fs::create_dir_all(parent)?; }
            let file = File::options().read(true).write(true).create(true).truncate(true).open(&full_path)?;
            streams.push(NewFileCombinedStream { file, size: dir_data.new_ref_size_list[i] as u64, });
        }
        Ok(streams)
    }
}

struct ChecksumSink<'a, 'b> {
    inner: &'a mut CombinedStream,
    checksum: Option<&'b mut Checksum>,
}

impl<'a, 'b> Write for ChecksumSink<'a, 'b> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write_all(buf)?;
        if let Some(c) = self.checksum.as_mut() { c.append(buf); }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> { self.inner.flush() }
}

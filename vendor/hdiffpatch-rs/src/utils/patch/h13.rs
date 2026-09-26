use std::fs::File;
use std::io::{Read, Write};
use crate::utils::compression::{IO_BUF_SIZE, get_clip_stream};
use crate::utils::mt::{PatchOptions, ThreadedReader};
use crate::utils::types::PatchCoreImpl;
use crate::utils::types::{HeaderInfo, PatchCore, SeekableRead};

pub struct PatchSingle {
    header_info: HeaderInfo,
    options: PatchOptions,
}

impl PatchSingle {
    pub fn new(header_info: HeaderInfo) -> Self {
        Self { header_info, options: PatchOptions::default() }
    }

    pub fn with_options(mut self, options: PatchOptions) -> Self {
        self.options = options;
        self
    }

    pub fn patch(&self, input_stream: &mut dyn SeekableRead, output_stream: &mut dyn Write, patch_path: &str) -> std::io::Result<()> {
        let padding: u64 = 0;
        let mut core = PatchCoreImpl::new(std::path::PathBuf::new(), std::path::PathBuf::new());
        self.start_patch_routine(input_stream, output_stream, &mut core, patch_path, padding)
    }

    fn start_patch_routine(&self, input_stream: &mut dyn SeekableRead, output_stream: &mut dyn Write, core: &mut PatchCoreImpl, patch_path: &str, padding: u64) -> std::io::Result<()> {
        let hi = &self.header_info;
        let ci = &hi.chunk_info;

        let f0 = File::open(patch_path)?;
        let f1 = File::open(patch_path)?;
        let f2 = File::open(patch_path)?;
        let f3 = File::open(patch_path)?;

        let mut offset = ci.head_end_pos as u64;
        let cover_padding = if ci.compress_cover_buf_size > 0 { padding } else { 0 };
        let (clip0, len0) = get_clip_stream(f0, hi.comp_mode.clone(), offset + cover_padding, ci.cover_buf_size as u64, ci.compress_cover_buf_size as u64, true)?;
        offset += len0;

        let rle_ctrl_padding = if ci.compress_rle_ctrl_buf_size > 0 { padding } else { 0 };
        let (clip1, len1) = get_clip_stream(f1, hi.comp_mode.clone(), offset + rle_ctrl_padding, ci.rle_ctrl_buf_size as u64, ci.compress_rle_ctrl_buf_size as u64, true)?;
        offset += len1;

        let rle_code_padding = if ci.compress_rle_code_buf_size > 0 { padding } else { 0 };
        let (clip2, len2) = get_clip_stream(f2, hi.comp_mode.clone(), offset + rle_code_padding, ci.rle_code_buf_size as u64, ci.compress_rle_code_buf_size as u64, true)?;
        offset += len2;

        let new_data_diff_padding = if ci.compress_new_data_diff_size > 0 { padding } else { 0 };
        let comp_diff_size = (ci.compress_new_data_diff_size as u64).saturating_sub(padding);
        let (clip3, _) = get_clip_stream(f3, hi.comp_mode.clone(), offset + new_data_diff_padding, ci.new_data_diff_size as u64, comp_diff_size, false)?;
        // Only clip3 is streamed lazily and large enough to be worth a thread;
        // the other three are already fully buffered in memory.
        let pipeline = self.options.pipeline(IO_BUF_SIZE);
        let clip3: Box<dyn Read + Send> = if pipeline.is_threaded() { Box::new(ThreadedReader::new(clip3, pipeline)) } else { clip3 };
        let mut clips: [Box<dyn Read + Send>; 4] = [clip0, clip1, clip2, clip3];
        core.uncover_buffer_clips_stream(&mut clips, input_stream, output_stream, hi);
        Ok(())
    }
}

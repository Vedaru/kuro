use std::fs::File;
use std::io::Write;
use crate::utils::compression::{IO_BUF_SIZE, get_clip_stream};
use crate::utils::mt::{PatchOptions, ThreadedReader};
use crate::utils::patch::step_engine::{StreamOld, patch_step_loop};
use crate::utils::types::{HeaderInfo, SeekableRead};

pub struct PatchSF {
    header_info: HeaderInfo,
    options: PatchOptions,
}

impl PatchSF {
    pub fn new(header_info: HeaderInfo) -> Self {
        Self { header_info, options: PatchOptions::default() }
    }

    pub fn with_options(mut self, options: PatchOptions) -> Self {
        self.options = options;
        self
    }

    pub fn patch(&self, input_stream: &mut dyn SeekableRead, output_stream: &mut dyn Write, patch_path: &str) -> std::io::Result<()> {
        let sci = &self.header_info.single_chunk_info;
        let (diff, _) = get_clip_stream(File::open(patch_path)?, self.header_info.comp_mode.clone(), sci.diff_data_pos as u64, sci.uncompressed_size as u64, sci.compressed_size as u64, false)?;
        let pipeline = self.options.pipeline(IO_BUF_SIZE);
        let mut diff: Box<dyn std::io::Read> = if pipeline.is_threaded() { Box::new(ThreadedReader::new(diff, pipeline)) } else { diff };

        let old_size = self.header_info.old_data_size as u64;
        let mut old = StreamOld::new(input_stream, old_size)?;

        let step_mem_size = self.header_info.step_mem_size as usize;
        let mut step_buf = vec![0u8; step_mem_size];
        let mut io_buf = vec![0u8; IO_BUF_SIZE];
        let mut written = 0u64;

        patch_step_loop(&mut diff, output_stream, &mut old, self.header_info.chunk_info.cover_count as u64, &mut step_buf, &mut io_buf, &mut written)?;

        let expected = self.header_info.new_data_size as u64;
        if written != expected { return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("[HDIFFSF20] produced {} bytes but header declares {}", written, expected))); }
        Ok(())
    }
}

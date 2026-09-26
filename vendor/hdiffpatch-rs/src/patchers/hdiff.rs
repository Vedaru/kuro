use std::fs::File;
use std::io::{BufWriter, Write};
use crate::utils::diff_info::{DiffFormat, DiffInfo, checksum_name, compression_name};
use crate::patchers::{HDiff, PatchOptions};
use crate::utils::compression::IO_BUF_SIZE;
use crate::utils::header::Header;
use crate::utils::mt::ThreadedWriter;
use crate::utils::patch::dir::PatchDir;
use crate::utils::patch::sf20::PatchSF;
use crate::utils::patch::h13::PatchSingle;
use crate::utils::patch::w26::PatchW;
use crate::utils::types::{DataReferenceInfo, HeaderInfo};

impl HDiff {
    /// Initialize the `HDIFF` patcher.
    pub fn new(source_path: String, diff_path: String, dest_path: String) -> Self {
        HDiff { source_path, diff_path, dest_path, options: PatchOptions::default() }
    }

    /// Overrides the default threading and pipeline memory budget.
    pub fn with_options(mut self, options: PatchOptions) -> Self {
        self.options = options;
        self
    }

    /// Get information about initialized `HDIFF` file.
    pub fn info(&self) -> Result<DiffInfo, Box<dyn std::error::Error>> {
        let mut file = File::open(&self.diff_path)?;
        let mut header: HeaderInfo = Default::default();
        let mut reference: DataReferenceInfo = Default::default();
        let is_dir = Header::try_parse_header_info(&mut file, &self.diff_path, &mut header, &mut reference)?;
        let is_directory = is_dir && header.is_input_dir && header.is_output_dir;

        let (format, window_count) = match (&header.window_diff_info, header.is_single_compressed_diff) {
            (Some(w), _) => (DiffFormat::HDiffW26, Some(w.window_count)),
            (None, true) => (DiffFormat::HDiffSf20, None),
            (None, false) => (DiffFormat::HDiff13, None),
        };

        Ok(DiffInfo {
            format,
            is_directory,
            compression: compression_name(&header.comp_mode),
            checksum: checksum_name(&header.checksum_mode),
            old_size: header.old_data_size.max(0) as u64,
            new_size: header.new_data_size.max(0) as u64,
            cover_count: header.chunk_info.cover_count.max(0) as u64,
            window_count,
            old_file_count: if is_directory { Some(reference.input_ref_file_count as u64) } else { None },
            new_file_count: if is_directory { Some(reference.output_ref_file_count as u64) } else { None },
        })
    }

    /// Apply initialized `HDIFF` patch.
    pub fn apply(&mut self) -> bool {
        match self.apply_inner() {
            Ok(()) => true,
            Err(e) => { eprintln!("[HDiff::apply] Error: {}", e); false }
        }
    }

    fn apply_inner(&self) -> Result<(), Box<dyn std::error::Error>> {
        let mut diff_file = File::open(&self.diff_path)?;
        let mut header_info: HeaderInfo = Default::default();
        let mut reference_info: DataReferenceInfo = Default::default();
        let is_dir_patch = Header::try_parse_header_info(&mut diff_file, &self.diff_path, &mut header_info, &mut reference_info)?;

        if is_dir_patch && header_info.is_input_dir && header_info.is_output_dir {
            let mut patcher = PatchDir::new(header_info, reference_info, self.diff_path.clone());
            patcher.set_options(self.options);
            patcher.patch(&self.source_path, &self.dest_path)?;
            return Ok(());
        }

        let mut old_file = File::open(&self.source_path)?;
        let old_len = old_file.metadata()?.len() as i64;
        if old_len != header_info.old_data_size { return Err(format!("[HDiff::apply] Input file size mismatch: expected {} bytes, got {} bytes", header_info.old_data_size, old_len).into()); }

        let out_file = File::create(&self.dest_path)?;
        let pipeline = self.options.pipeline(IO_BUF_SIZE);

        if pipeline.is_threaded() {
            let mut out = ThreadedWriter::new(Box::new(out_file), pipeline);
            self.run(&header_info, &mut old_file, old_len, &mut out)?;
            out.finish()?;
        } else {
            let mut out = BufWriter::with_capacity(IO_BUF_SIZE, out_file);
            self.run(&header_info, &mut old_file, old_len, &mut out)?;
            out.flush()?;
        }
        Ok(())
    }

    fn run(&self, header_info: &HeaderInfo, old_file: &mut File, old_len: i64, out: &mut dyn Write) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(info) = header_info.window_diff_info.clone() {
            let diff_file = File::open(&self.diff_path)?;
            PatchW::new(info).with_options(self.options).patch(diff_file, 0, old_file, old_len as u64, out, Some(header_info.comp_mode.clone()), Some(std::path::Path::new(&self.source_path)))?;
        } else if header_info.is_single_compressed_diff {
            PatchSF::new(header_info.clone()).with_options(self.options).patch(old_file, out, &self.diff_path)?;
        } else {
            PatchSingle::new(header_info.clone()).with_options(self.options).patch(old_file, out, &self.diff_path)?;
        }
        Ok(())
    }
}

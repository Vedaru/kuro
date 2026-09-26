use std::fs::create_dir_all;
use std::path::Path;
use crate::utils::diff_info::{DiffFormat, DiffInfo, compression_name};
use crate::patchers::{KrDiff, PatchOptions};
use crate::utils::patch::krdir::{parse_hd13, parse_hd19};
use crate::utils::create::krdir::{KrCreateOptions, create};
use crate::utils::patch::krdir::KrPatchDir;

/*
WARNING: This shit is extremely cursed and is modification of standard HDiff format, it is not something you should use it can break and go to fuckshit anytime...
This only exists to support TwintailLauncher's use case and is very hacked to hell compared to actual standard HDiff patching part
HERE BE DRAGONS you are warned!!!
#FuckKuroGames btw
*/

impl KrDiff {
    /// Initialize the `HDIFF` kuro modified patcher.
    pub fn new(source_path: String, diff_path: String, dest_path: String) -> Self {
        KrDiff { source_path, diff_path, dest_path, options: PatchOptions::default() }
    }

    /// Overrides the default threading and pipeline memory budget.
    pub fn with_options(mut self, options: PatchOptions) -> Self {
        self.options = options;
        self
    }

    /// Get information about initialized `HDIFF` kuro modified file.
    pub fn info(&self) -> Result<DiffInfo, Box<dyn std::error::Error>> {
        let mut file = std::fs::File::open(&self.diff_path)?;
        let hd19 = parse_hd19(&mut file)?;
        let hd13 = parse_hd13(&mut file)?;

        Ok(DiffInfo {
            format: DiffFormat::KrDiff,
            is_directory: true,
            compression: compression_name(&hd19.comp_mode),
            checksum: hd19.checksum_type.clone(),
            old_size: hd19.old_ref_size,
            new_size: hd19.new_ref_size,
            cover_count: hd13.covers.len() as u64,
            window_count: None,
            old_file_count: Some(hd19.head.old_files.len() as u64),
            new_file_count: Some(hd19.head.new_files.len() as u64),
        })
    }

    /// Builds a KrDiff from `source_path` (old tree) to `dest_path` (new tree),
    /// writing it to `diff_path`. This is Kuro's format, not vanilla hdiffz
    pub fn create(&mut self) -> bool {
        match create(&self.source_path, &self.dest_path, &self.diff_path, &KrCreateOptions::default()) {
            Ok(()) => true,
            Err(e) => { eprintln!("[KrDiff::create] Error: {}", e); false }
        }
    }

    /// Apply initialized `HDIFF` kuro modified patch.
    pub fn apply(&mut self) -> bool {
        match self.apply_inner() {
            Ok(()) => true,
            Err(e) => { eprintln!("[KrDiff::apply] Error: {}", e); false }
        }
    }

    fn apply_inner(&self) -> Result<(), Box<dyn std::error::Error>> {
        let src = Path::new(&self.source_path);
        let diffp = Path::new(&self.diff_path);

        let dst = std::path::PathBuf::from(&self.dest_path);
        if !src.exists() || !src.is_dir() { return Err(format!("[KrDiff] Source path {} does not exist or is not a directory", src.display()).into()); }
        if !diffp.exists() || !diffp.is_file() { return Err(format!("[KrDiff] Diff file {} does not exist", diffp.display()).into()); }
        if !dst.exists() { create_dir_all(&dst)?; }

        let patcher = KrPatchDir::new(self.diff_path.clone()).with_options(self.options);
        patcher.patch(src.to_str().unwrap_or(""), dst.to_str().unwrap_or(""))?;
        Ok(())
    }
}

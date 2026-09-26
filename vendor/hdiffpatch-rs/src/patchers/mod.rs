pub mod hdiff;
pub mod krdiff;

pub use crate::utils::diff_info::{DiffFormat, DiffInfo};
pub use crate::utils::mt::PatchOptions;

pub struct KrDiff {
    source_path: String,
    diff_path: String,
    dest_path: String,
    pub(crate) options: PatchOptions,
}

pub struct HDiff {
    source_path: String,
    diff_path: String,
    dest_path: String,
    pub(crate) options: PatchOptions,
}
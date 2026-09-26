use std::fs::File;
use std::io::{Read, Write};
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CompressionMode {
    #[default]
    Nocomp,
    Zstd,
    Zlib,
    Bz2,
    Lzma,
    Lzma2,
    Unsupported(String),
}

impl FromStr for CompressionMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_lowercase().as_str() {
            "" | "nocomp" => CompressionMode::Nocomp,
            "zstd" => CompressionMode::Zstd,
            "zlib" | "pzlib" => CompressionMode::Zlib,
            "bz2" | "bzip2" | "pbz2" | "pbzip2" => CompressionMode::Bz2,
            "lzma" => CompressionMode::Lzma,
            "lzma2" => CompressionMode::Lzma2,
            _ => CompressionMode::Unsupported(s.to_string()),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum ChecksumMode {
    #[default]
    Nochecksum,
    Crc32,
    Adler32,
    Adler64,
    Fadler32,
    Fadler64,
    Fadler128,
    Md5,
    Sha1,
    Sha256,
    Sha512,
    Blake3,
    Xxh3,
    Xxh128,
    Unsupported(String),
}

impl FromStr for ChecksumMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_lowercase().as_str() {
            "" | "nochecksum" => ChecksumMode::Nochecksum,
            "crc32" => ChecksumMode::Crc32,
            "adler32" => ChecksumMode::Adler32,
            "adler64" => ChecksumMode::Adler64,
            "fadler32" => ChecksumMode::Fadler32,
            "fadler64" => ChecksumMode::Fadler64,
            "fadler128" => ChecksumMode::Fadler128,
            "md5" => ChecksumMode::Md5,
            "sha1" => ChecksumMode::Sha1,
            "sha256" => ChecksumMode::Sha256,
            "sha512" => ChecksumMode::Sha512,
            "blake3" => ChecksumMode::Blake3,
            "xxh3" => ChecksumMode::Xxh3,
            "xxh128" => ChecksumMode::Xxh128,
            _ => ChecksumMode::Unsupported(s.to_string()),
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct HeaderInfo {
    pub comp_mode: CompressionMode,
    pub checksum_mode: ChecksumMode,
    pub is_input_dir: bool,
    pub is_output_dir: bool,
    pub is_single_compressed_diff: bool,
    pub patch_path: String,
    pub header_magic: String,
    pub step_mem_size: i64,
    pub dir_data_is_compressed: bool,
    pub old_data_size: i64,
    pub new_data_size: i64,
    pub compressed_count: i64,
    pub single_chunk_info: DiffSingleChunkInfo,
    pub chunk_info: DiffChunkInfo,
    pub(crate) window_diff_info: Option<crate::utils::patch::w26::WindowDiffInfo>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DataReferenceInfo {
    pub input_dir_count: i64,
    pub input_ref_file_count: i64,
    pub input_ref_file_size: i64,
    pub input_sum_size: i64,
    pub output_dir_count: i64,
    pub output_ref_file_count: i64,
    pub output_ref_file_size: i64,
    pub output_sum_size: i64,
    pub same_file_pair_count: i64,
    pub same_file_size: i64,
    pub new_execute_count: i32,
    pub private_reserved_data_size: i64,
    pub private_extern_data_size: i64,
    pub private_extern_data_offset: i64,
    pub extern_data_offset: i64,
    pub extern_data_size: i64,
    pub compress_size_begin_pos: i64,
    pub checksum_byte_size: u8,
    pub checksum_offset: i64,
    pub checksums: Vec<u8>,
    pub head_data_size: i64,
    pub head_data_offset: i64,
    pub head_data_compressed_size: i64,
    pub hdiff_data_offset: i64,
    pub hdiff_data_size: i64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DiffSingleChunkInfo {
    pub uncompressed_size: i64,
    pub compressed_size: i64,
    pub diff_data_pos: i64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DiffChunkInfo {
    pub types_end_pos: i64,
    pub cover_count: i64,
    pub compress_size_begin_pos: i64,
    pub cover_buf_size: i64,
    pub compress_cover_buf_size: i64,
    pub rle_ctrl_buf_size: i64,
    pub compress_rle_ctrl_buf_size: i64,
    pub rle_code_buf_size: i64,
    pub compress_rle_code_buf_size: i64,
    pub new_data_diff_size: i64,
    pub compress_new_data_diff_size: i64,
    pub head_end_pos: i64,
    pub cover_end_pos: i64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DirectoryReferencePair {
    pub old_utf8_path_list: Vec<String>,
    pub new_utf8_path_list: Vec<String>,
    pub old_ref_list: Vec<i64>,
    pub new_ref_list: Vec<i64>,
    pub new_ref_size_list: Vec<i64>,
    pub data_same_pair_list: Vec<PairIndexReference>,
    pub new_execute_list: Vec<i64>,
}

#[derive(Debug, Clone)]
pub(crate) struct PairIndexReference {
    pub(crate) new_index: i64,
    pub(crate) old_index: i64,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RleRefClip {
    pub mem_copy_length: i64,
    pub mem_set_length: i64,
    pub mem_set_value: u8,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CoverHeader {
    pub old_pos: i64,
    pub new_pos: i64,
    pub cover_length: i64,
    pub next_cover_index: i64,
}

impl CoverHeader {
    pub fn new(old_pos: i64, new_pos: i64, cover_length: i64, next_cover_index: i64) -> Self {
        Self { old_pos, new_pos, cover_length, next_cover_index }
    }
}

pub(crate) trait PatchCore {
    fn set_directory_reference_pair(&mut self, pair: DirectoryReferencePair);
    fn uncover_buffer_clips_stream(&mut self, clips: &mut [Box<dyn Read + Send>], input_stream: &mut dyn SeekableRead, output_stream: &mut dyn Write, header_info: &HeaderInfo);
}

pub(crate) trait SeekableRead: Read + std::io::Seek {}
impl<T: Read + std::io::Seek> SeekableRead for T {}

pub(crate) struct PatchCoreImpl {
    pub path_input: std::path::PathBuf,
    pub path_output: std::path::PathBuf,
    pub dir_reference_pair: Option<DirectoryReferencePair>,
}

pub(crate) struct CombinedStream {
    pub(crate) streams: Vec<File>,
    pub(crate) start_positions: Vec<u64>,
    pub(crate) position: u64,
    pub(crate) index: usize,
    pub(crate) total_length: u64,
}

pub struct NewFileCombinedStream {
    pub(crate) file: File,
    pub(crate) size: u64,
}

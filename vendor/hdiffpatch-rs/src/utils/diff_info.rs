    use std::fmt;

/// Which on-the-wire diff format a patch file uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffFormat {
    /// Classic four-clip stream diff.
    HDiff13,
    /// Single-compressed step diff.
    HDiffSf20,
    /// Window diff.
    HDiffW26,
    /// Kuro's modified directory diff. Not produced by upstream `hdiffz`.
    KrDiff,
}

impl fmt::Display for DiffFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            DiffFormat::HDiff13 => "HDIFF13",
            DiffFormat::HDiffSf20 => "HDIFFSF20",
            DiffFormat::HDiffW26 => "HDIFFW26",
            DiffFormat::KrDiff => "KRDIFF",
        };
        f.write_str(s)
    }
}

/// Everything a caller can learn about a patch without applying it.
///
/// For a directory diff, `format` describes the payload carried inside the
/// HDIFF19 container and `is_directory` is true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffInfo {
    pub format: DiffFormat,
    pub is_directory: bool,
    /// Compression plugin name, empty when the payload is stored uncompressed.
    pub compression: String,
    /// Checksum plugin name, empty when the patch carries no digest.
    pub checksum: String,
    /// Bytes the input is expected to be.
    pub old_size: u64,
    /// Bytes the output will be.
    pub new_size: u64,
    pub cover_count: u64,
    /// HDIFFW26 only.
    pub window_count: Option<u64>,
    /// Directory diffs only: files referenced on each side.
    pub old_file_count: Option<u64>,
    pub new_file_count: Option<u64>,
}

impl fmt::Display for DiffInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.format)?;
        if self.is_directory {
            f.write_str(" (directory)")?;
        }
        write!(
            f,
            " old={} new={} covers={} compression={} checksum={}",
            self.old_size,
            self.new_size,
            self.cover_count,
            if self.compression.is_empty() { "none" } else { &self.compression },
            if self.checksum.is_empty() { "none" } else { &self.checksum },
        )?;
        if let Some(w) = self.window_count {
            write!(f, " windows={}", w)?;
        }
        if let (Some(o), Some(n)) = (self.old_file_count, self.new_file_count) {
            write!(f, " files={}->{}", o, n)?;
        }
        Ok(())
    }
}

pub(crate) fn compression_name(mode: &crate::utils::types::CompressionMode) -> String {
    use crate::utils::types::CompressionMode as C;
    match mode {
        C::Nocomp => String::new(),
        C::Zstd => "zstd".into(),
        C::Zlib => "zlib".into(),
        C::Bz2 => "bz2".into(),
        C::Lzma => "lzma".into(),
        C::Lzma2 => "lzma2".into(),
        C::Unsupported(name) => name.clone(),
    }
}

pub(crate) fn checksum_name(mode: &crate::utils::types::ChecksumMode) -> String {
    use crate::utils::types::ChecksumMode as K;
    match mode {
        K::Nochecksum => String::new(),
        K::Crc32 => "crc32".into(),
        K::Adler32 => "adler32".into(),
        K::Adler64 => "adler64".into(),
        K::Fadler32 => "fadler32".into(),
        K::Fadler64 => "fadler64".into(),
        K::Fadler128 => "fadler128".into(),
        K::Md5 => "md5".into(),
        K::Sha1 => "sha1".into(),
        K::Sha256 => "sha256".into(),
        K::Sha512 => "sha512".into(),
        K::Blake3 => "blake3".into(),
        K::Xxh3 => "xxh3".into(),
        K::Xxh128 => "xxh128".into(),
        K::Unsupported(name) => name.clone(),
    }
}

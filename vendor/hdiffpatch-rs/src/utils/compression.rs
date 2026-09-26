use std::fs::File;
use std::io::{BufReader, Cursor, Error, ErrorKind, Read, Seek, SeekFrom, Take};
use crate::utils::types::CompressionMode;

pub(crate) const IO_BUF_SIZE: usize = 256 * 1024;
const ZSTD_WINDOW_LOG_MAX: u32 = 31;

fn unsupported(what: &str) -> Error {
    Error::new(ErrorKind::Unsupported, format!("[compression] {} decompression is not supported", what))
}

fn bad(msg: String) -> Error {
    Error::new(ErrorKind::InvalidData, msg)
}

fn clip_reader(mut file: File, start: u64, len: u64) -> std::io::Result<BufReader<Take<File>>> {
    file.seek(SeekFrom::Start(start))?;
    Ok(BufReader::with_capacity(IO_BUF_SIZE, file.take(len)))
}

pub(crate) fn get_clip_stream(file: File, comp_mode: CompressionMode, start: u64, length: u64, comp_length: u64, is_buffered: bool) -> std::io::Result<(Box<dyn Read + Send>, u64)> {
    let file_bytes = if comp_length > 0 { comp_length } else { length };

    if comp_length == 0 {
        // Stored: no plugin involved regardless of the declared compress type.
        let reader = clip_reader(file, start, length)?;
        if is_buffered {
            let mut buf = Vec::with_capacity(length as usize);
            let mut reader = reader;
            reader.read_to_end(&mut buf)?;
            return Ok((Box::new(Cursor::new(buf)), file_bytes));
        }
        return Ok((Box::new(reader), file_bytes));
    }

    let reader = clip_reader(file, start, comp_length)?;
    let decoded = open_decompressor(comp_mode, reader, length)?;

    if is_buffered {
        let mut buf = Vec::with_capacity(length as usize);
        let mut decoded = decoded;
        decoded.read_to_end(&mut buf)?;
        return Ok((Box::new(Cursor::new(buf)), file_bytes));
    }
    Ok((decoded, file_bytes))
}

/// Builds the decoder chain for `comp_mode`. Shared by the two public wrappers
/// below, which differ only in the trait object they box into.
macro_rules! decoder_chain {
    ($comp_mode:expr, $reader:ident, $uncompressed_len:expr) => {
        match $comp_mode {
            CompressionMode::Nocomp => Ok(Box::new($reader) as _),
            CompressionMode::Zstd => {
                let mut decoder = zstd::stream::read::Decoder::new($reader)?;
                decoder.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(ZSTD_WINDOW_LOG_MAX))?;
                Ok(Box::new(decoder) as _)
            }
            CompressionMode::Zlib => {
                let mut wb = [0u8; 1];
                $reader.read_exact(&mut wb)?;
                let window_bits = wb[0] as i8;
                if !(-15..0).contains(&window_bits) {
                    return Err(bad(format!("[compression] unsupported zlib windowBits: {}", window_bits)));
                }
                Ok(Box::new(flate2::read::DeflateDecoder::new($reader)) as _)
            }
            CompressionMode::Bz2 => Ok(Box::new(bzip2::read::BzDecoder::new($reader)) as _),
            CompressionMode::Lzma => {
                let mut n = [0u8; 1];
                $reader.read_exact(&mut n)?;
                let mut props = vec![0u8; n[0] as usize];
                $reader.read_exact(&mut props)?;

                let mut filters = liblzma::stream::Filters::new();
                filters.lzma1_properties(&props).map_err(|e| bad(format!("[compression] invalid lzma props: {}", e)))?;
                let stream = liblzma::stream::Stream::new_raw_decoder(&filters).map_err(|e| bad(format!("[compression] lzma decoder init failed: {}", e)))?;
                Ok(Box::new(liblzma::read::XzDecoder::new_stream($reader, stream).take($uncompressed_len)) as _)
            }
            CompressionMode::Lzma2 => {
                let mut prop = [0u8; 1];
                $reader.read_exact(&mut prop)?;

                let mut filters = liblzma::stream::Filters::new();
                filters.lzma2_properties(&prop).map_err(|e| bad(format!("[compression] invalid lzma2 props: {}", e)))?;
                let stream = liblzma::stream::Stream::new_raw_decoder(&filters).map_err(|e| bad(format!("[compression] lzma2 decoder init failed: {}", e)))?;
                Ok(Box::new(liblzma::read::XzDecoder::new_stream($reader, stream).take($uncompressed_len)) as _)
            }
            CompressionMode::Unsupported(ref name) => Err(unsupported(name)),
        }
    };
}

/// Decoder over an owned stream that can move to another thread.
pub(crate) fn open_decompressor<R: Read + Send + 'static>(comp_mode: CompressionMode, mut reader: R, uncompressed_len: u64) -> std::io::Result<Box<dyn Read + Send>> {
    decoder_chain!(comp_mode, reader, uncompressed_len)
}

/// Decoder over a borrowed stream, for callers that parse a section in place and
/// then need the underlying reader back.
pub(crate) fn open_decompressor_local<'a, R: Read + 'a>(comp_mode: CompressionMode, mut reader: R, uncompressed_len: u64) -> std::io::Result<Box<dyn Read + 'a>> {
    decoder_chain!(comp_mode, reader, uncompressed_len)
}

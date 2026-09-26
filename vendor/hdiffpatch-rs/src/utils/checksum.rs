use crate::utils::fadler_tables::{FADLER128_TABLE, FADLER32_TABLE, FADLER64_TABLE};
use crate::utils::types::ChecksumMode;

const ADLER32_BASE: u32 = 65521;
const ADLER64_BASE: u64 = 0xFFFFFFFB;

pub(crate) enum Checksum {
    Crc32(crc32fast::Hasher),
    Adler32 { a: u32, b: u32 },
    Adler64 { a: u64, b: u64 },
    Fadler32 { a: u32, b: u32 },
    Fadler64 { a: u64, b: u64 },
    Fadler128 { a: u64, b: u64 },
    Md5(md5::Md5),
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    Sha512(sha2::Sha512),
    Blake3(Box<blake3::Hasher>),
    Xxh3(Box<xxhash_rust::xxh3::Xxh3>),
    Xxh128(Box<xxhash_rust::xxh3::Xxh3>),
}

impl ChecksumMode {
    pub(crate) fn checksum_byte_size(&self) -> Option<usize> {
        Some(match self {
            ChecksumMode::Nochecksum => 0,
            ChecksumMode::Crc32 | ChecksumMode::Adler32 | ChecksumMode::Fadler32 => 4,
            ChecksumMode::Adler64 | ChecksumMode::Fadler64 | ChecksumMode::Xxh3 => 8,
            ChecksumMode::Fadler128 | ChecksumMode::Md5 | ChecksumMode::Xxh128 => 16,
            ChecksumMode::Sha1 => 20,
            ChecksumMode::Sha256 | ChecksumMode::Blake3 => 32,
            ChecksumMode::Sha512 => 64,
            ChecksumMode::Unsupported(_) => return None,
        })
    }

    pub(crate) fn new_checksum(&self) -> Option<Checksum> {
        use digest::Digest;
        Some(match self {
            ChecksumMode::Nochecksum | ChecksumMode::Unsupported(_) => return None,
            ChecksumMode::Crc32 => Checksum::Crc32(crc32fast::Hasher::new()),
            ChecksumMode::Adler32 => Checksum::Adler32 { a: 1, b: 0 },
            ChecksumMode::Adler64 => Checksum::Adler64 { a: 1, b: 0 },
            ChecksumMode::Fadler32 => Checksum::Fadler32 { a: 1, b: 0 },
            ChecksumMode::Fadler64 => Checksum::Fadler64 { a: 1, b: 0 },
            ChecksumMode::Fadler128 => Checksum::Fadler128 { a: 1, b: 0 },
            ChecksumMode::Md5 => Checksum::Md5(md5::Md5::new()),
            ChecksumMode::Sha1 => Checksum::Sha1(sha1::Sha1::new()),
            ChecksumMode::Sha256 => Checksum::Sha256(sha2::Sha256::new()),
            ChecksumMode::Sha512 => Checksum::Sha512(sha2::Sha512::new()),
            ChecksumMode::Blake3 => Checksum::Blake3(Box::new(blake3::Hasher::new())),
            ChecksumMode::Xxh3 => Checksum::Xxh3(Box::new(xxhash_rust::xxh3::Xxh3::new())),
            ChecksumMode::Xxh128 => Checksum::Xxh128(Box::new(xxhash_rust::xxh3::Xxh3::new())),
        })
    }
}

impl Checksum {
    pub(crate) fn append(&mut self, data: &[u8]) {
        use digest::Digest;
        match self {
            Checksum::Crc32(h) => h.update(data),
            Checksum::Adler32 { a, b } => {
                for &v in data {
                    *a = (*a + v as u32) % ADLER32_BASE;
                    *b = (*b + *a) % ADLER32_BASE;
                }
            }
            Checksum::Adler64 { a, b } => {
                for &v in data {
                    *a = (*a + v as u64) % ADLER64_BASE;
                    *b = (*b + *a) % ADLER64_BASE;
                }
            }
            // "fast adler": the byte is mapped through a scramble table and
            // accumulated without modular reduction (`_fast_adler_append`).
            Checksum::Fadler32 { a, b } => {
                for &v in data {
                    *a = a.wrapping_add(FADLER32_TABLE[v as usize] as u32);
                    *b = b.wrapping_add(*a);
                }
            }
            Checksum::Fadler64 { a, b } => {
                for &v in data {
                    *a = a.wrapping_add(FADLER64_TABLE[v as usize] as u64);
                    *b = b.wrapping_add(*a);
                }
            }
            Checksum::Fadler128 { a, b } => {
                for &v in data {
                    *a = a.wrapping_add(FADLER128_TABLE[v as usize]);
                    *b = b.wrapping_add(*a);
                }
            }
            Checksum::Md5(h) => h.update(data),
            Checksum::Sha1(h) => h.update(data),
            Checksum::Sha256(h) => h.update(data),
            Checksum::Sha512(h) => h.update(data),
            Checksum::Blake3(h) => { h.update(data); }
            Checksum::Xxh3(h) | Checksum::Xxh128(h) => h.update(data),
        }
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        use digest::Digest;
        match self {
            Checksum::Crc32(h) => h.finalize().to_le_bytes().to_vec(),
            // `adler | (sum << half_bit)`; the modular variants already fit.
            Checksum::Adler32 { a, b } => (a | (b << 16)).to_le_bytes().to_vec(),
            Checksum::Adler64 { a, b } => (a | (b << 32)).to_le_bytes().to_vec(),
            Checksum::Fadler32 { a, b } => ((a & 0xFFFF) | (b << 16)).to_le_bytes().to_vec(),
            Checksum::Fadler64 { a, b } => ((a as u32 as u64) | (b << 32)).to_le_bytes().to_vec(),
            // adler128_t is written as two little-endian u64s: adler then sum.
            Checksum::Fadler128 { a, b } => {
                let mut out = a.to_le_bytes().to_vec();
                out.extend_from_slice(&b.to_le_bytes());
                out
            }
            Checksum::Md5(h) => h.finalize().to_vec(),
            Checksum::Sha1(h) => h.finalize().to_vec(),
            Checksum::Sha256(h) => h.finalize().to_vec(),
            Checksum::Sha512(h) => h.finalize().to_vec(),
            Checksum::Blake3(h) => h.finalize().as_bytes().to_vec(),
            Checksum::Xxh3(h) => h.digest().to_le_bytes().to_vec(),
            Checksum::Xxh128(h) => h.digest128().to_le_bytes().to_vec(),
        }
    }
}

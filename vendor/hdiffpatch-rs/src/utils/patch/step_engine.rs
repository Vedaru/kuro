use std::io::{Error, ErrorKind, Read, SeekFrom, Write};
use crate::utils::types::SeekableRead;

#[inline(always)]
fn bad(msg: &'static str) -> Error {
    Error::new(ErrorKind::InvalidData, msg)
}

pub(crate) trait OldSource {
    fn size(&self) -> u64;
    fn read_at(&mut self, pos: u64, buf: &mut [u8]) -> std::io::Result<()>;
}

pub(crate) struct StreamOld<'a> {
    inner: &'a mut dyn SeekableRead,
    pos: u64,
    size: u64,
}

impl<'a> StreamOld<'a> {
    pub fn new(inner: &'a mut dyn SeekableRead, size: u64) -> std::io::Result<Self> {
        let pos = inner.seek(SeekFrom::Start(0))?;
        Ok(Self { inner, pos, size })
    }
}

impl<'a> OldSource for StreamOld<'a> {
    #[inline]
    fn size(&self) -> u64 { self.size }

    fn read_at(&mut self, pos: u64, buf: &mut [u8]) -> std::io::Result<()> {
        if pos != self.pos {
            self.inner.seek(SeekFrom::Start(pos))?;
            self.pos = pos;
        }
        self.inner.read_exact(buf)?;
        self.pos += buf.len() as u64;
        Ok(())
    }
}

pub(crate) struct MemOld<'a> {
    pub data: &'a [u8],
}

impl<'a> OldSource for MemOld<'a> {
    #[inline]
    fn size(&self) -> u64 { self.data.len() as u64 }

    #[inline]
    fn read_at(&mut self, pos: u64, buf: &mut [u8]) -> std::io::Result<()> {
        let start = pos as usize;
        let end = start.checked_add(buf.len()).ok_or_else(|| bad("old read overflow"))?;
        if end > self.data.len() { return Err(bad("old read out of window bounds")); }
        buf.copy_from_slice(&self.data[start..end]);
        Ok(())
    }
}

#[inline]
pub(crate) fn unpack_uint_with_tag(p: &mut &[u8], tag_bit: u32) -> std::io::Result<u64> {
    let s = *p;
    if s.is_empty() { return Err(bad("packed uint: unexpected end of data")); }
    let code = s[0];
    let mask = (1u8 << (7 - tag_bit)).wrapping_sub(1);
    let mut value = (code & mask) as u64;
    let mut i = 1usize;
    if (code & (1u8 << (7 - tag_bit))) != 0 {
        loop {
            if (value >> (64 - 7)) != 0 { return Err(bad("packed uint: value overflow")); }
            if i >= s.len() { return Err(bad("packed uint: unexpected end of data")); }
            let code = s[i];
            i += 1;
            value = (value << 7) | (code & 0x7F) as u64;
            if (code & 0x80) == 0 { break; }
        }
    }
    *p = &s[i..];
    Ok(value)
}

#[inline]
pub(crate) fn unpack_uint(p: &mut &[u8]) -> std::io::Result<u64> {
    unpack_uint_with_tag(p, 0)
}

pub(crate) fn read_packed_uint(r: &mut dyn Read) -> std::io::Result<u64> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    let mut value = (b[0] & 0x7F) as u64;
    if (b[0] & 0x80) != 0 {
        loop {
            if (value >> (64 - 7)) != 0 { return Err(bad("packed uint: value overflow")); }
            r.read_exact(&mut b)?;
            value = (value << 7) | (b[0] & 0x7F) as u64;
            if (b[0] & 0x80) == 0 { break; }
        }
    }
    Ok(value)
}

#[inline]
pub(crate) fn read_sign_pos_by_last_pos(p: &mut &[u8], last_pos: &mut u64) -> std::io::Result<()> {
    if p.is_empty() { return Err(bad("signed pos: unexpected end of data")); }
    let is_neg = (p[0] >> 7) != 0;
    let abs_delta = unpack_uint_with_tag(p, 1)?;
    if is_neg {
        *last_pos = last_pos.checked_sub(abs_delta).ok_or_else(|| bad("signed pos underflow"))?;
    } else {
        *last_pos = last_pos.checked_add(abs_delta).ok_or_else(|| bad("signed pos overflow"))?;
    }
    Ok(())
}

struct Covers<'a> {
    cache: &'a [u8],
    old_pos: u64,
    new_pos: u64,
    length: u64,
    last_old_end: u64,
    last_new_end: u64,
}

impl<'a> Covers<'a> {
    #[inline]
    fn new(cache: &'a [u8]) -> Self {
        Self { cache, old_pos: 0, new_pos: 0, length: 0, last_old_end: 0, last_new_end: 0 }
    }

    #[inline]
    fn has_next(&self) -> bool { !self.cache.is_empty() }

    #[inline]
    fn next(&mut self) -> std::io::Result<()> {
        let sign_neg = (self.cache[0] >> 7) != 0;
        self.last_old_end = self.old_pos.wrapping_add(self.length);
        self.last_new_end = self.new_pos.wrapping_add(self.length);

        let delta = unpack_uint_with_tag(&mut self.cache, 1)?;
        self.old_pos = if sign_neg {
            self.last_old_end.checked_sub(delta).ok_or_else(|| bad("cover oldPos underflow"))?
        } else {
            self.last_old_end.checked_add(delta).ok_or_else(|| bad("cover oldPos overflow"))?
        };

        let inc_new = unpack_uint(&mut self.cache)?;
        self.new_pos = self.last_new_end.checked_add(inc_new).ok_or_else(|| bad("cover newPos overflow"))?;
        self.length = unpack_uint(&mut self.cache)?;
        Ok(())
    }
}

struct Rle0Decoder<'a> {
    code: &'a [u8],
    len0: u64,
    lenv: u64,
    need_decode0: bool,
}

impl<'a> Rle0Decoder<'a> {
    #[inline]
    fn new(code: &'a [u8]) -> Self {
        Self { code, len0: 0, lenv: 0, need_decode0: true }
    }

    fn add(&mut self, data: &mut [u8]) -> std::io::Result<()> {
        let mut dp = 0usize;
        let mut rem = data.len() as u64;
        while rem > 0 {
            if self.len0 > 0 {
                // A zero run: the old bytes pass through unchanged.
                let take = self.len0.min(rem);
                self.len0 -= take;
                dp += take as usize;
                rem -= take;
            } else if self.lenv > 0 {
                let take = self.lenv.min(rem) as usize;
                let src = &self.code[..take];
                let dst = &mut data[dp..dp + take];
                for i in 0..take { dst[i] = dst[i].wrapping_add(src[i]); }
                self.code = &self.code[take..];
                self.lenv -= take as u64;
                dp += take;
                rem -= take as u64;
            } else if self.need_decode0 {
                self.need_decode0 = false;
                self.len0 = unpack_uint(&mut self.code)?;
            } else {
                self.need_decode0 = true;
                let lenv = unpack_uint(&mut self.code)?;
                if lenv > self.code.len() as u64 { return Err(bad("rle0: value run exceeds code buffer")); }
                self.lenv = lenv;
            }
        }
        Ok(())
    }
}

fn add_old_with_rle0(out: &mut dyn Write, rle0: &mut Rle0Decoder, old: &mut dyn OldSource, mut old_pos: u64, mut len: u64, io_buf: &mut [u8]) -> std::io::Result<()> {
    while len > 0 {
        let step = (io_buf.len() as u64).min(len) as usize;
        let chunk = &mut io_buf[..step];
        old.read_at(old_pos, chunk)?;
        rle0.add(chunk)?;
        out.write_all(chunk)?;
        old_pos += step as u64;
        len -= step as u64;
    }
    Ok(())
}

fn copy_from_diff(diff: &mut dyn Read, out: &mut dyn Write, mut n: u64, io_buf: &mut [u8]) -> std::io::Result<()> {
    while n > 0 {
        let take = (io_buf.len() as u64).min(n) as usize;
        diff.read_exact(&mut io_buf[..take])?;
        out.write_all(&io_buf[..take])?;
        n -= take as u64;
    }
    Ok(())
}

pub(crate) fn patch_step_loop(diff: &mut dyn Read, out: &mut dyn Write, old: &mut dyn OldSource, mut cover_count: u64, step_buf: &mut [u8], io_buf: &mut [u8], written: &mut u64) -> std::io::Result<()> {
    let step_mem_size = step_buf.len() as u64;
    let old_size = old.size();

    while cover_count > 0 {
        let buf_cover_size = read_packed_uint(diff)?;
        let buf_rle_size = read_packed_uint(diff)?;
        if buf_cover_size > step_mem_size || buf_rle_size > step_mem_size || buf_cover_size + buf_rle_size > step_mem_size {
            return Err(bad("step buffers exceed stepMemSize"));
        }
        let step_end = (buf_cover_size + buf_rle_size) as usize;
        diff.read_exact(&mut step_buf[..step_end])?;

        let (covers_cache, rle_cache) = step_buf[..step_end].split_at(buf_cover_size as usize);
        let mut covers = Covers::new(covers_cache);
        let mut rle0 = Rle0Decoder::new(rle_cache);

        while covers.has_next() {
            covers.next()?;
            if cover_count == 0 { return Err(bad("more covers encoded than coverCount")); }

            if covers.new_pos > covers.last_new_end {
                // Bytes with no match in old data: taken verbatim from the diff stream.
                let gap = covers.new_pos - covers.last_new_end;
                copy_from_diff(diff, out, gap, io_buf)?;
                *written += gap;
            }

            cover_count -= 1;
            if covers.length > 0 {
                if covers.old_pos > old_size || covers.length > old_size - covers.old_pos {
                    return Err(bad("cover reads past end of old data"));
                }
                add_old_with_rle0(out, &mut rle0, old, covers.old_pos, covers.length, io_buf)?;
                *written += covers.length;
            } else if cover_count != 0 {
                // A zero-length cover is only legal as the final one.
                return Err(bad("zero-length cover before the last cover"));
            }
        }
    }
    Ok(())
}

//! Read-only, random-access byte sources with bounded-memory cursors.
//!
//! Evidence is only ever opened for reading. Parsers never load a whole
//! candidate into memory; they pull bytes through a [`Cursor`] with a fixed
//! buffer.

use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::Arc;

/// Positional, read-only access to a byte source.
pub trait ReadAt: Send + Sync {
    fn len(&self) -> u64;

    /// Read up to `buf.len()` bytes at `offset`. Returns 0 at end of source.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Read as many bytes as available (up to `buf.len()`), retrying short reads.
pub fn read_full_at(src: &dyn ReadAt, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        match src.read_at(offset + done as u64, &mut buf[done..]) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

/// Read exactly `len` bytes or fail.
pub fn read_vec_at(src: &dyn ReadAt, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut v = vec![0u8; len];
    let n = read_full_at(src, offset, &mut v)?;
    if n != len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("short read at offset {offset}: wanted {len}, got {n}"),
        ));
    }
    Ok(v)
}

/// A file opened strictly read-only.
pub struct FileSource {
    file: File,
    len: u64,
}

impl FileSource {
    pub fn open(path: &Path) -> io::Result<Self> {
        // `File::open` requests read access only; we never request write or
        // delete access to evidence.
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        Ok(FileSource { file, len })
    }
}

impl ReadAt for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    #[cfg(unix)]
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        use std::os::unix::fs::FileExt;
        if offset >= self.len {
            return Ok(0);
        }
        self.file.read_at(buf, offset)
    }

    #[cfg(windows)]
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        use std::os::windows::fs::FileExt;
        if offset >= self.len {
            return Ok(0);
        }
        self.file.seek_read(buf, offset)
    }
}

/// In-memory source, used by tests and for small containers.
#[derive(Clone)]
pub struct MemSource(pub Arc<Vec<u8>>);

impl MemSource {
    pub fn new(data: Vec<u8>) -> Self {
        MemSource(Arc::new(data))
    }
}

impl ReadAt for MemSource {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let data = &self.0;
        if offset >= data.len() as u64 {
            return Ok(0);
        }
        let start = offset as usize;
        let n = buf.len().min(data.len() - start);
        buf[..n].copy_from_slice(&data[start..start + n]);
        Ok(n)
    }
}

/// A window `[base, base+len)` of another source (e.g. one partition).
pub struct SliceSource {
    inner: Arc<dyn ReadAt>,
    base: u64,
    len: u64,
}

impl SliceSource {
    pub fn new(inner: Arc<dyn ReadAt>, base: u64, len: u64) -> Self {
        let avail = inner.len().saturating_sub(base);
        SliceSource {
            inner,
            base,
            len: len.min(avail),
        }
    }
}

impl ReadAt for SliceSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if offset >= self.len {
            return Ok(0);
        }
        let max = ((self.len - offset) as usize).min(buf.len());
        self.inner.read_at(self.base + offset, &mut buf[..max])
    }
}

impl<T: ReadAt + ?Sized> ReadAt for Arc<T> {
    fn len(&self) -> u64 {
        (**self).len()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        (**self).read_at(offset, buf)
    }
}

/// Default cursor buffer size (64 KiB).
pub const CURSOR_BUFFER: usize = 64 * 1024;

/// Sequential reader over a [`ReadAt`] with a fixed-size buffer and a hard
/// end limit. Used by the format parsers so that memory use is independent
/// of candidate size.
pub struct Cursor<'a> {
    src: &'a dyn ReadAt,
    pos: u64,
    limit: u64,
    buf: Vec<u8>,
    buf_start: u64,
    buf_len: usize,
}

impl<'a> Cursor<'a> {
    /// Cursor starting at `start` that will never read at or beyond `limit`.
    pub fn new(src: &'a dyn ReadAt, start: u64, limit: u64) -> Self {
        let limit = limit.min(src.len());
        Cursor {
            src,
            pos: start,
            limit,
            buf: vec![0u8; CURSOR_BUFFER],
            buf_start: start,
            buf_len: 0,
        }
    }

    pub fn position(&self) -> u64 {
        self.pos
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.pos)
    }

    pub fn at_end(&self) -> bool {
        self.pos >= self.limit
    }

    fn fill(&mut self) -> io::Result<bool> {
        if self.pos >= self.limit {
            return Ok(false);
        }
        let want = ((self.limit - self.pos) as usize).min(self.buf.len());
        let n = read_full_at(self.src, self.pos, &mut self.buf[..want])?;
        self.buf_start = self.pos;
        self.buf_len = n;
        Ok(n > 0)
    }

    #[inline]
    fn in_buffer(&self) -> bool {
        self.pos >= self.buf_start && self.pos < self.buf_start + self.buf_len as u64
    }

    /// Next byte, or `None` at the limit / end of source.
    #[inline]
    pub fn next_u8(&mut self) -> io::Result<Option<u8>> {
        if !self.in_buffer() && !self.fill()? {
            return Ok(None);
        }
        let b = self.buf[(self.pos - self.buf_start) as usize];
        self.pos += 1;
        Ok(Some(b))
    }

    /// Peek without advancing.
    #[inline]
    pub fn peek_u8(&mut self) -> io::Result<Option<u8>> {
        if !self.in_buffer() && !self.fill()? {
            return Ok(None);
        }
        Ok(Some(self.buf[(self.pos - self.buf_start) as usize]))
    }

    /// Currently buffered bytes starting at the cursor position (may be
    /// empty only at end). Used for fast scanning loops.
    pub fn buffered(&mut self) -> io::Result<&[u8]> {
        if !self.in_buffer() && !self.fill()? {
            return Ok(&[]);
        }
        let s = (self.pos - self.buf_start) as usize;
        Ok(&self.buf[s..self.buf_len])
    }

    /// Advance by `n` bytes (bounded by the limit). Returns bytes skipped.
    pub fn skip(&mut self, n: u64) -> u64 {
        let k = n.min(self.remaining());
        self.pos += k;
        k
    }

    pub fn seek(&mut self, pos: u64) {
        self.pos = pos.min(self.limit);
    }

    /// Read exactly `out.len()` bytes; `Ok(false)` if the limit is hit first.
    pub fn read_exact(&mut self, out: &mut [u8]) -> io::Result<bool> {
        let mut i = 0;
        while i < out.len() {
            let chunk = self.buffered()?;
            if chunk.is_empty() {
                return Ok(false);
            }
            let n = chunk.len().min(out.len() - i);
            out[i..i + n].copy_from_slice(&chunk[..n]);
            i += n;
            self.pos += n as u64;
        }
        Ok(true)
    }

    pub fn read_u16_be(&mut self) -> io::Result<Option<u16>> {
        let mut b = [0u8; 2];
        Ok(self.read_exact(&mut b)?.then(|| u16::from_be_bytes(b)))
    }

    pub fn read_u32_be(&mut self) -> io::Result<Option<u32>> {
        let mut b = [0u8; 4];
        Ok(self.read_exact(&mut b)?.then(|| u32::from_be_bytes(b)))
    }

    pub fn read_u32_le(&mut self) -> io::Result<Option<u32>> {
        let mut b = [0u8; 4];
        Ok(self.read_exact(&mut b)?.then(|| u32::from_le_bytes(b)))
    }

    pub fn read_u64_be(&mut self) -> io::Result<Option<u64>> {
        let mut b = [0u8; 8];
        Ok(self.read_exact(&mut b)?.then(|| u64::from_be_bytes(b)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_respects_limit_and_buffers() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let src = MemSource::new(data.clone());
        let mut c = Cursor::new(&src, 10, 150_000);
        let mut count = 0u64;
        while let Some(b) = c.next_u8().unwrap() {
            assert_eq!(b, data[(10 + count) as usize]);
            count += 1;
        }
        assert_eq!(count, 150_000 - 10);
        assert!(c.at_end());
    }

    #[test]
    fn slice_source_window() {
        let src: Arc<dyn ReadAt> = Arc::new(MemSource::new((0..100u8).collect()));
        let s = SliceSource::new(src, 10, 20);
        let mut b = [0u8; 50];
        let n = read_full_at(&s, 15, &mut b).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&b[..5], &[25, 26, 27, 28, 29]);
    }
}

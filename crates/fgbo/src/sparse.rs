//! Sparse, retry-driven reading for environments without blocking I/O
//! (wasm/browser): a `Read + Seek` over a chunk cache that reports the
//! first missing range instead of blocking.
//!
//! Protocol: run any decoder operation; if it fails and
//! [`MissingRange::take`] yields a range, fetch those bytes (over-fetching
//! is fine), [`ChunkCache::insert`] them, and retry the operation. Each
//! round trip makes strict progress, so the loop terminates.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};

/// Shared chunk store keyed by absolute byte offset.
#[derive(Clone, Default)]
pub struct ChunkCache {
    inner: Arc<Mutex<BTreeMap<u64, Vec<u8>>>>,
}

impl ChunkCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert fetched bytes at `offset`. Overlaps are fine; a chunk that
    /// is fully contained in an existing one is dropped.
    pub fn insert(&self, offset: u64, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        let mut m = self.inner.lock().unwrap();
        if let Some((s, c)) = m.range(..=offset).next_back() {
            if *s + c.len() as u64 >= offset + bytes.len() as u64 {
                return; // fully covered already
            }
        }
        m.insert(offset, bytes);
    }

    /// Total cached bytes (diagnostics).
    pub fn cached_bytes(&self) -> u64 {
        self.inner
            .lock()
            .unwrap()
            .values()
            .map(|v| v.len() as u64)
            .sum()
    }

    /// Copy bytes at `pos` into `buf`; `None` when `pos` is not cached.
    /// May return fewer bytes than requested (chunk boundary).
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> Option<usize> {
        let m = self.inner.lock().unwrap();
        let (start, chunk) = m.range(..=pos).next_back()?;
        let off = (pos - start) as usize;
        if off >= chunk.len() {
            return None;
        }
        let n = buf.len().min(chunk.len() - off);
        buf[..n].copy_from_slice(&chunk[off..off + n]);
        Some(n)
    }
}

/// The first missing `(offset, len)` recorded by a failed read.
#[derive(Clone, Default)]
pub struct MissingRange {
    inner: Arc<Mutex<Option<(u64, u64)>>>,
}

impl MissingRange {
    pub fn new() -> Self {
        Self::default()
    }
    /// Consume the recorded range.
    pub fn take(&self) -> Option<(u64, u64)> {
        self.inner.lock().unwrap().take()
    }
    /// Non-consuming view of the recorded range.
    pub fn peek(&self) -> Option<(u64, u64)> {
        *self.inner.lock().unwrap()
    }
    pub fn set(&self, offset: u64, len: u64) {
        *self.inner.lock().unwrap() = Some((offset, len));
    }
}

/// `Read + Seek` over a [`ChunkCache`]; reads outside the cache record
/// the missing range and fail with `ErrorKind::WouldBlock`.
pub struct SparseReader {
    cache: ChunkCache,
    missing: MissingRange,
    len: u64,
    pos: u64,
}

impl SparseReader {
    pub fn new(cache: ChunkCache, missing: MissingRange, len: u64) -> Self {
        SparseReader {
            cache,
            missing,
            len,
            pos: 0,
        }
    }
}

impl Read for SparseReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.len.saturating_sub(self.pos);
        let want = (buf.len() as u64).min(remaining) as usize;
        if want == 0 {
            return Ok(0);
        }
        match self.cache.read_at(self.pos, &mut buf[..want]) {
            Some(n) => {
                self.pos += n as u64;
                Ok(n)
            }
            None => {
                self.missing.set(self.pos, want as u64);
                Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "range not cached yet",
                ))
            }
        }
    }
}

impl Seek for SparseReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new = match pos {
            SeekFrom::Start(o) => o as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => self.len as i64 + d,
        };
        if new < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn miss_then_hit() {
        let cache = ChunkCache::new();
        let missing = MissingRange::new();
        let mut r = SparseReader::new(cache.clone(), missing.clone(), 100);

        let mut buf = [0u8; 8];
        r.seek(SeekFrom::Start(10)).unwrap();
        assert!(r.read_exact(&mut buf).is_err());
        assert_eq!(missing.take(), Some((10, 8)));

        cache.insert(8, (0..32).collect());
        r.seek(SeekFrom::Start(10)).unwrap();
        r.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [2, 3, 4, 5, 6, 7, 8, 9]);
        assert!(missing.take().is_none());
    }

    #[test]
    fn read_spans_chunks() {
        let cache = ChunkCache::new();
        let missing = MissingRange::new();
        cache.insert(0, vec![1; 4]);
        cache.insert(4, vec![2; 4]);
        let mut r = SparseReader::new(cache, missing, 8);
        let mut buf = [0u8; 8];
        r.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [1, 1, 1, 1, 2, 2, 2, 2]);
    }

    #[test]
    fn eof_is_clean() {
        let cache = ChunkCache::new();
        cache.insert(0, vec![9; 10]);
        let mut r = SparseReader::new(cache, MissingRange::new(), 10);
        let mut all = Vec::new();
        r.read_to_end(&mut all).unwrap();
        assert_eq!(all.len(), 10);
    }
}

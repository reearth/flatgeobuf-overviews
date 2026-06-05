//! Blocking HTTP range reader: a `Read + Seek` adapter over HTTP range
//! requests, so [`crate::FgboReader`] works on remote files unchanged.
//!
//! Every `read_exact` the decoder issues maps to exactly one
//! `Range: bytes=...` request (the decoder already coalesces reads), so
//! `IoStats` request counts equal real HTTP request counts.

use crate::error::{Error, Result};
use std::io::{Read, Seek, SeekFrom};

/// `Read + Seek` over a remote file via HTTP range requests.
pub struct HttpRangeReader {
    url: String,
    agent: ureq::Agent,
    len: u64,
    pos: u64,
}

impl HttpRangeReader {
    /// Open `url`, learning the file length from a 1-byte range probe
    /// (also verifies that the server supports range requests).
    pub fn open(url: &str) -> Result<Self> {
        let agent = ureq::Agent::new_with_defaults();
        let res = agent
            .get(url)
            .header("Range", "bytes=0-0")
            .call()
            .map_err(|e| Error::InvalidInput(format!("HTTP request failed: {e}")))?;
        if res.status() != 206 {
            return Err(Error::InvalidInput(format!(
                "server does not support range requests (status {})",
                res.status()
            )));
        }
        let content_range = res
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Error::InvalidInput("missing Content-Range header".into()))?;
        // "bytes 0-0/12345"
        let len: u64 = content_range
            .rsplit('/')
            .next()
            .and_then(|s| s.trim().parse().ok())
            .ok_or_else(|| {
                Error::InvalidInput(format!("unparsable Content-Range: {content_range}"))
            })?;
        Ok(HttpRangeReader {
            url: url.to_string(),
            agent,
            len,
            pos: 0,
        })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Read for HttpRangeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.len.saturating_sub(self.pos);
        let want = (buf.len() as u64).min(remaining);
        if want == 0 {
            return Ok(0);
        }
        let end = self.pos + want - 1;
        let res = self
            .agent
            .get(&self.url)
            .header("Range", format!("bytes={}-{}", self.pos, end))
            .call()
            .map_err(|e| std::io::Error::other(format!("range request failed: {e}")))?;
        if res.status() != 206 {
            return Err(std::io::Error::other(format!(
                "expected 206 Partial Content, got {}",
                res.status()
            )));
        }
        let mut body = res.into_body().into_reader();
        let mut filled = 0usize;
        while filled < want as usize {
            let n = body.read(&mut buf[filled..want as usize])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        self.pos += filled as u64;
        Ok(filled)
    }
}

impl Seek for HttpRangeReader {
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

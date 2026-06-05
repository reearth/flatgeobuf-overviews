//! FGBO binary format: footer and directory.
//!
//! Layout of an FGBO file:
//!
//! ```text
//! ┌─────────────────────────────────────────────┐
//! │ plain FlatGeobuf (magic/header/index/data)  │  <- valid fgb body
//! ├─────────────────────────────────────────────┤
//! │ sentinel (u32 = 0xFFFFFFFF)                 │  <- stops EOF-style readers
//! │ section: importance sidecar                 │
//! │ section: overview level 0..n (mini fgb)     │
//! │ section: segments (mini fgb of fragments)   │
//! │ directory                                   │
//! │ footer (fixed 32 bytes)                     │
//! └─────────────────────────────────────────────┘
//! ```
//!
//! All integers are little-endian.

use crate::error::{Error, Result};

/// Footer magic: "FGBOVRV1".
pub const FOOTER_MAGIC: [u8; 8] = *b"FGBOVRV1";
/// Fixed footer size in bytes.
pub const FOOTER_SIZE: usize = 32;
/// Sentinel placed at the start of the extension area. Read as a feature
/// length prefix by a non-conforming sequential reader, it is an absurd
/// size (4 GiB - 1) that triggers an immediate error instead of garbage.
pub const SENTINEL: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];

pub const DIRECTORY_VERSION: u8 = 1;

/// Fixed-size footer at the end of the file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Footer {
    pub dir_offset: u64,
    pub dir_size: u64,
    pub dir_crc32: u32,
}

impl Footer {
    pub fn encode(&self) -> [u8; FOOTER_SIZE] {
        let mut buf = [0u8; FOOTER_SIZE];
        buf[0..8].copy_from_slice(&FOOTER_MAGIC);
        buf[8..16].copy_from_slice(&self.dir_offset.to_le_bytes());
        buf[16..24].copy_from_slice(&self.dir_size.to_le_bytes());
        buf[24..28].copy_from_slice(&self.dir_crc32.to_le_bytes());
        // bytes 28..32 reserved (zero)
        buf
    }

    /// Decode the footer from the last 32 bytes of a file.
    /// Returns `Err(Error::NotFgbo)` if the magic does not match.
    pub fn decode(buf: &[u8]) -> Result<Footer> {
        if buf.len() < FOOTER_SIZE {
            return Err(Error::NotFgbo);
        }
        let buf = &buf[buf.len() - FOOTER_SIZE..];
        if buf[0..8] != FOOTER_MAGIC {
            return Err(Error::NotFgbo);
        }
        Ok(Footer {
            dir_offset: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            dir_size: u64::from_le_bytes(buf[16..24].try_into().unwrap()),
            dir_crc32: u32::from_le_bytes(buf[24..28].try_into().unwrap()),
        })
    }
}

/// Importance sidecar section entry.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportanceEntry {
    pub offset: u64,
    pub size: u64,
    pub feature_count: u64,
}

/// Overview level section entry. The section content is a complete
/// mini-FlatGeobuf (magic + header + index + data).
#[derive(Debug, Clone, PartialEq)]
pub struct OverviewEntry {
    pub offset: u64,
    pub size: u64,
    /// Recommended zoom range (inclusive).
    pub min_zoom: u8,
    pub max_zoom: u8,
    /// Quantized squared simplification tolerance of this level
    /// (see [`crate::importance::quantize_sqdist`]).
    pub tolerance_q: u16,
    pub feature_count: u64,
}

/// Segments section entry. The section content is a complete mini-FlatGeobuf
/// holding fragments of large features clipped at the `zbase` grid.
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentsEntry {
    pub offset: u64,
    pub size: u64,
    /// Zoom of the clipping grid.
    pub zbase: u8,
    /// Vertex-count threshold above which a feature was segmented.
    pub v_max: u32,
    pub fragment_count: u64,
    /// Ordinals (file order) of body features that were segmented.
    /// Sorted ascending. Readers must exclude these from the body at z >= zbase.
    pub segmented_ordinals: Vec<u64>,
}

/// FGBO directory: locations and metadata of all extension sections.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Directory {
    pub importance: Option<ImportanceEntry>,
    /// Sorted by min_zoom ascending.
    pub overviews: Vec<OverviewEntry>,
    pub segments: Option<SegmentsEntry>,
    /// Tool version / build info (determinism check aid).
    pub build_info: String,
}

impl Directory {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(DIRECTORY_VERSION);

        match &self.importance {
            Some(e) => {
                buf.push(1);
                buf.extend_from_slice(&e.offset.to_le_bytes());
                buf.extend_from_slice(&e.size.to_le_bytes());
                buf.extend_from_slice(&e.feature_count.to_le_bytes());
            }
            None => buf.push(0),
        }

        buf.push(self.overviews.len() as u8);
        for e in &self.overviews {
            buf.extend_from_slice(&e.offset.to_le_bytes());
            buf.extend_from_slice(&e.size.to_le_bytes());
            buf.push(e.min_zoom);
            buf.push(e.max_zoom);
            buf.extend_from_slice(&e.tolerance_q.to_le_bytes());
            buf.extend_from_slice(&e.feature_count.to_le_bytes());
        }

        match &self.segments {
            Some(e) => {
                buf.push(1);
                buf.extend_from_slice(&e.offset.to_le_bytes());
                buf.extend_from_slice(&e.size.to_le_bytes());
                buf.push(e.zbase);
                buf.extend_from_slice(&e.v_max.to_le_bytes());
                buf.extend_from_slice(&e.fragment_count.to_le_bytes());
                buf.extend_from_slice(&(e.segmented_ordinals.len() as u64).to_le_bytes());
                for o in &e.segmented_ordinals {
                    buf.extend_from_slice(&o.to_le_bytes());
                }
            }
            None => buf.push(0),
        }

        let info = self.build_info.as_bytes();
        buf.extend_from_slice(&(info.len() as u16).to_le_bytes());
        buf.extend_from_slice(info);
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Directory> {
        let mut r = Cursor { buf, pos: 0 };
        let version = r.u8()?;
        if version != DIRECTORY_VERSION {
            return Err(Error::Format(format!(
                "unsupported directory version {version}"
            )));
        }

        let importance = if r.u8()? == 1 {
            Some(ImportanceEntry {
                offset: r.u64()?,
                size: r.u64()?,
                feature_count: r.u64()?,
            })
        } else {
            None
        };

        let n = r.u8()? as usize;
        let mut overviews = Vec::with_capacity(n);
        for _ in 0..n {
            overviews.push(OverviewEntry {
                offset: r.u64()?,
                size: r.u64()?,
                min_zoom: r.u8()?,
                max_zoom: r.u8()?,
                tolerance_q: r.u16()?,
                feature_count: r.u64()?,
            });
        }

        let segments = if r.u8()? == 1 {
            let offset = r.u64()?;
            let size = r.u64()?;
            let zbase = r.u8()?;
            let v_max = r.u32()?;
            let fragment_count = r.u64()?;
            let big_n = r.u64()? as usize;
            let mut segmented_ordinals = Vec::with_capacity(big_n);
            for _ in 0..big_n {
                segmented_ordinals.push(r.u64()?);
            }
            Some(SegmentsEntry {
                offset,
                size,
                zbase,
                v_max,
                fragment_count,
                segmented_ordinals,
            })
        } else {
            None
        };

        let info_len = r.u16()? as usize;
        let build_info = String::from_utf8(r.bytes(info_len)?.to_vec())
            .map_err(|_| Error::Format("invalid build_info utf8".into()))?;

        Ok(Directory {
            importance,
            overviews,
            segments,
            build_info,
        })
    }

    pub fn crc32(encoded: &[u8]) -> u32 {
        crc32fast::hash(encoded)
    }

    /// Pick the overview level for zoom `z`, if any covers it.
    pub fn overview_for_zoom(&self, z: u8) -> Option<&OverviewEntry> {
        self.overviews
            .iter()
            .find(|e| z >= e.min_zoom && z <= e.max_zoom)
    }
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            return Err(Error::Format("directory truncated".into()));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.bytes(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_dir() -> Directory {
        Directory {
            importance: Some(ImportanceEntry {
                offset: 1000,
                size: 200,
                feature_count: 10,
            }),
            overviews: vec![
                OverviewEntry {
                    offset: 1200,
                    size: 300,
                    min_zoom: 0,
                    max_zoom: 4,
                    tolerance_q: 30000,
                    feature_count: 5,
                },
                OverviewEntry {
                    offset: 1500,
                    size: 400,
                    min_zoom: 5,
                    max_zoom: 8,
                    tolerance_q: 40000,
                    feature_count: 9,
                },
            ],
            segments: Some(SegmentsEntry {
                offset: 1900,
                size: 500,
                zbase: 12,
                v_max: 16384,
                fragment_count: 42,
                segmented_ordinals: vec![3, 7],
            }),
            build_info: "fgbo 0.1.0".into(),
        }
    }

    #[test]
    fn directory_roundtrip() {
        let dir = sample_dir();
        let enc = dir.encode();
        let dec = Directory::decode(&enc).unwrap();
        assert_eq!(dir, dec);
    }

    #[test]
    fn footer_roundtrip() {
        let f = Footer {
            dir_offset: 123456,
            dir_size: 789,
            dir_crc32: 0xDEADBEEF,
        };
        let enc = f.encode();
        assert_eq!(enc.len(), FOOTER_SIZE);
        let dec = Footer::decode(&enc).unwrap();
        assert_eq!(f, dec);
    }

    #[test]
    fn footer_rejects_plain_fgb() {
        let buf = [0u8; FOOTER_SIZE];
        assert!(matches!(Footer::decode(&buf), Err(Error::NotFgbo)));
    }

    #[test]
    fn overview_selection() {
        let dir = sample_dir();
        assert_eq!(dir.overview_for_zoom(0).unwrap().min_zoom, 0);
        assert_eq!(dir.overview_for_zoom(4).unwrap().max_zoom, 4);
        assert_eq!(dir.overview_for_zoom(6).unwrap().min_zoom, 5);
        assert!(dir.overview_for_zoom(9).is_none());
    }
}

//! Low-level access to a FlatGeobuf "section": a complete fgb byte range
//! (magic + header + optional index + data) embedded at an arbitrary offset
//! of a larger file. Used both for the FGBO body and for overview/segments
//! sections (FGBO reuses fgb encodings wholesale).
//!
//! All reads are explicit seek+read so that I/O can be counted; the same
//! access pattern maps 1:1 to HTTP range requests.

use crate::error::{Error, Result};
use flatgeobuf::packed_r_tree::{PackedRTree, SearchResultItem};
use flatgeobuf::{
    size_prefixed_root_as_feature, size_prefixed_root_as_header, ColumnType, Feature, GeometryType,
    Header,
};
use geozero::geo_types::GeoWriter;
use geozero::ColumnValue;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicU64, Ordering};

/// fgb magic ("fgb" 3 "fgb" 0).
pub const FGB_MAGIC: [u8; 8] = [b'f', b'g', b'b', 3, b'f', b'g', b'b', 0];

/// I/O statistics: emulates "how many range requests / bytes would this be
/// over HTTP".
#[derive(Debug, Default)]
pub struct IoStats {
    pub requests: AtomicU64,
    pub bytes: AtomicU64,
}

impl IoStats {
    pub fn record(&self, bytes: u64) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
    pub fn reset(&self) {
        self.requests.store(0, Ordering::Relaxed);
        self.bytes.store(0, Ordering::Relaxed);
    }
}

/// Read `len` bytes at absolute `offset`, recording stats.
pub fn read_range<R: Read + Seek>(
    r: &mut R,
    stats: &IoStats,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>> {
    r.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    stats.record(len as u64);
    Ok(buf)
}

/// A `Read + Seek` window over `[offset, offset+len)` of an underlying
/// reader, with stats counting. Needed by `PackedRTree::stream_search`.
pub struct WindowReader<'a, R> {
    inner: &'a mut R,
    stats: &'a IoStats,
    offset: u64,
    len: u64,
    pos: u64,
}

impl<'a, R: Read + Seek> WindowReader<'a, R> {
    pub fn new(inner: &'a mut R, stats: &'a IoStats, offset: u64, len: u64) -> Self {
        WindowReader {
            inner,
            stats,
            offset,
            len,
            pos: 0,
        }
    }
}

impl<R: Read + Seek> Read for WindowReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.len.saturating_sub(self.pos) as usize;
        let want = buf.len().min(remaining);
        if want == 0 {
            return Ok(0);
        }
        self.inner.seek(SeekFrom::Start(self.offset + self.pos))?;
        let n = self.inner.read(&mut buf[..want])?;
        self.pos += n as u64;
        self.stats.record(n as u64);
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for WindowReader<'_, R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new = match pos {
            SeekFrom::Start(o) => o as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => self.len as i64 + d,
        };
        if new < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before start of window",
            ));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

/// Parsed metadata of one fgb section.
pub struct FgbSection {
    /// Absolute offset of the section (start of magic bytes).
    pub offset: u64,
    /// Section size in bytes (0 = unknown / until EOF).
    pub size: u64,
    header_buf: Vec<u8>,
    /// Absolute offset of the index.
    pub index_offset: u64,
    pub index_size: u64,
    /// Absolute offset of the data (feature) section.
    pub data_offset: u64,
    pub features_count: u64,
    pub node_size: u16,
}

impl FgbSection {
    /// Open an fgb section at `offset` in `r`.
    pub fn open<R: Read + Seek>(
        r: &mut R,
        stats: &IoStats,
        offset: u64,
        size: u64,
    ) -> Result<FgbSection> {
        let head = read_range(r, stats, offset, 12)?;
        if head[0..3] != FGB_MAGIC[0..3] || head[4..7] != FGB_MAGIC[4..7] {
            return Err(Error::Format(format!(
                "no fgb magic at section offset {offset}"
            )));
        }
        let header_size = u32::from_le_bytes(head[8..12].try_into().unwrap()) as usize;
        if header_size > 10 * 1024 * 1024 {
            return Err(Error::Format(format!("header too large: {header_size}")));
        }
        let mut header_buf = vec![0u8; header_size + 4];
        header_buf[0..4].copy_from_slice(&head[8..12]);
        let rest = read_range(r, stats, offset + 12, header_size)?;
        header_buf[4..].copy_from_slice(&rest);

        let header = size_prefixed_root_as_header(&header_buf)
            .map_err(|e| Error::Format(format!("invalid fgb header: {e}")))?;
        let features_count = header.features_count();
        let node_size = header.index_node_size();

        let index_offset = offset + 8 + 4 + header_size as u64;
        let index_size = if node_size > 0 && features_count > 0 {
            PackedRTree::index_size(features_count as usize, node_size) as u64
        } else {
            0
        };
        let data_offset = index_offset + index_size;

        Ok(FgbSection {
            offset,
            size,
            header_buf,
            index_offset,
            index_size,
            data_offset,
            features_count,
            node_size,
        })
    }

    pub fn header(&self) -> Header<'_> {
        // verified in open()
        unsafe { flatgeobuf::size_prefixed_root_as_header_unchecked(&self.header_buf) }
    }

    pub fn has_index(&self) -> bool {
        self.index_size > 0
    }

    /// Spatial bbox search over the section index. Results are sorted by
    /// data-relative byte offset; `index` is the feature ordinal in file
    /// order.
    ///
    /// Small indexes are fetched in a single range read; large ones are
    /// traversed with per-node reads (`PackedRTree::stream_search`).
    pub fn search<R: Read + Seek>(
        &self,
        r: &mut R,
        stats: &IoStats,
        min_x: f64,
        min_y: f64,
        max_x: f64,
        max_y: f64,
    ) -> Result<Vec<SearchResultItem>> {
        if !self.has_index() {
            return Err(Error::Format("section has no spatial index".into()));
        }
        let mut items = if self.index_size <= SMALL_INDEX_BYTES {
            let buf = read_range(r, stats, self.index_offset, self.index_size as usize)?;
            PackedRTree::stream_search(
                &mut std::io::Cursor::new(buf),
                self.features_count as usize,
                self.node_size,
                min_x,
                min_y,
                max_x,
                max_y,
            )?
        } else {
            // stream_search reads node items field by field (8 B at a
            // time); buffer one node group per underlying range read
            let win = WindowReader::new(r, stats, self.index_offset, self.index_size);
            let group_bytes = (self.node_size as usize).max(2) * 40;
            let mut buffered = std::io::BufReader::with_capacity(group_bytes, win);
            PackedRTree::stream_search(
                &mut buffered,
                self.features_count as usize,
                self.node_size,
                min_x,
                min_y,
                max_x,
                max_y,
            )?
        };
        items.sort_by_key(|i| i.offset);
        Ok(items)
    }

    /// Read one size-prefixed feature record at data-relative `offset`.
    /// Returns the full size-prefixed buffer.
    pub fn read_feature<R: Read + Seek>(
        &self,
        r: &mut R,
        stats: &IoStats,
        offset: u64,
    ) -> Result<Vec<u8>> {
        let abs = self.data_offset + offset;
        let len_buf = read_range(r, stats, abs, 4)?;
        let len = u32::from_le_bytes(len_buf[0..4].try_into().unwrap()) as usize;
        if len > MAX_FEATURE_BYTES {
            return Err(Error::Format(format!("feature too large: {len}")));
        }
        let mut buf = Vec::with_capacity(len + 4);
        buf.extend_from_slice(&len_buf);
        buf.extend_from_slice(&read_range(r, stats, abs + 4, len)?);
        // validate
        size_prefixed_root_as_feature(&buf)
            .map_err(|e| Error::Format(format!("invalid feature record: {e}")))?;
        Ok(buf)
    }

    /// Read many feature records with range coalescing: consecutive hits
    /// whose gap is at most [`COALESCE_GAP_BYTES`] share one range read
    /// (over-reading the skipped bytes), the way an HTTP reader would
    /// batch requests. Returns `(ordinal, size-prefixed buffer)` pairs in
    /// offset order.
    pub fn read_features<R: Read + Seek>(
        &self,
        r: &mut R,
        stats: &IoStats,
        items: &[SearchResultItem],
    ) -> Result<Vec<(u64, Vec<u8>)>> {
        let section_end = if self.size > 0 {
            self.offset + self.size
        } else {
            u64::MAX
        };
        let mut out = Vec::with_capacity(items.len());

        let mut group_start = 0usize;
        while group_start < items.len() {
            // grow the group while gaps stay small
            let mut group_end = group_start + 1;
            while group_end < items.len()
                && (items[group_end].offset - items[group_end - 1].offset) as u64
                    <= COALESCE_GAP_BYTES
            {
                group_end += 1;
            }
            let group = &items[group_start..group_end];
            group_start = group_end;

            let first = self.data_offset + group[0].offset as u64;
            let last_rel = group.last().unwrap().offset as u64;
            let last_abs = self.data_offset + last_rel;
            // read through the last feature's length prefix, padding small
            // groups up to a minimum request size (one round trip beats a
            // tiny read + follow-up for the body)
            let read_end = (last_abs + 4)
                .max(first + MIN_GROUP_READ_BYTES)
                .min(section_end.max(last_abs + 4));
            let mut buf = read_range(r, stats, first, (read_end - first) as usize)?;

            // the last feature may extend beyond what we have
            let last_in_buf = (last_abs - first) as usize;
            let last_len =
                u32::from_le_bytes(buf[last_in_buf..last_in_buf + 4].try_into().unwrap()) as usize;
            if last_len > MAX_FEATURE_BYTES {
                return Err(Error::Format(format!("feature too large: {last_len}")));
            }
            let needed = last_in_buf + 4 + last_len;
            if needed > buf.len() {
                let more = read_range(r, stats, first + buf.len() as u64, needed - buf.len())?;
                buf.extend_from_slice(&more);
            }

            for item in group {
                let rel = (self.data_offset + item.offset as u64 - first) as usize;
                if rel + 4 > buf.len() {
                    return Err(Error::Format("coalesced read out of bounds".into()));
                }
                let len = u32::from_le_bytes(buf[rel..rel + 4].try_into().unwrap()) as usize;
                if len > MAX_FEATURE_BYTES || rel + 4 + len > buf.len() {
                    return Err(Error::Format("feature record out of bounds".into()));
                }
                let fbuf = buf[rel..rel + 4 + len].to_vec();
                size_prefixed_root_as_feature(&fbuf)
                    .map_err(|e| Error::Format(format!("invalid feature record: {e}")))?;
                out.push((item.index as u64, fbuf));
            }
        }
        Ok(out)
    }
}

/// Maximum gap between consecutive hits sharing one coalesced range read.
pub const COALESCE_GAP_BYTES: u64 = 64 * 1024;
/// Indexes up to this size are fetched whole in one request.
pub const SMALL_INDEX_BYTES: u64 = 256 * 1024;
/// Minimum coalesced request size (avoids a 4-byte read + follow-up).
pub const MIN_GROUP_READ_BYTES: u64 = 16 * 1024;
/// Sanity cap on a single feature record.
const MAX_FEATURE_BYTES: usize = 512 * 1024 * 1024;

/// Parse a size-prefixed feature buffer (as returned by
/// [`FgbSection::read_feature`]).
pub fn feature_root(buf: &[u8]) -> Result<Feature<'_>> {
    size_prefixed_root_as_feature(buf).map_err(|e| Error::Format(format!("invalid feature: {e}")))
}

/// Convert a feature's geometry to geo-types (lon/lat, as stored).
pub fn feature_to_geo(
    feature: &Feature,
    geometry_type: GeometryType,
) -> Result<geo_types::Geometry<f64>> {
    let geom = feature
        .geometry()
        .ok_or_else(|| Error::Format("feature without geometry".into()))?;
    let mut writer = GeoWriter::new();
    flatgeobuf::read_geometry(&mut writer, &geom, geometry_type)?;
    writer
        .take_geometry()
        .ok_or_else(|| Error::Format("geometry conversion produced nothing".into()))
}

/// An owned property value (FlatGeobuf column types).
#[derive(Debug, Clone, PartialEq)]
pub enum PropValue {
    Bool(bool),
    Byte(i8),
    UByte(u8),
    Short(i16),
    UShort(u16),
    Int(i32),
    UInt(u32),
    Long(i64),
    ULong(u64),
    Float(f32),
    Double(f64),
    String(String),
    Json(String),
    DateTime(String),
    Binary(Vec<u8>),
}

impl PropValue {
    /// Borrowed geozero ColumnValue view (for writing through geozero).
    pub fn as_column_value(&self) -> ColumnValue<'_> {
        match self {
            PropValue::Bool(v) => ColumnValue::Bool(*v),
            PropValue::Byte(v) => ColumnValue::Byte(*v),
            PropValue::UByte(v) => ColumnValue::UByte(*v),
            PropValue::Short(v) => ColumnValue::Short(*v),
            PropValue::UShort(v) => ColumnValue::UShort(*v),
            PropValue::Int(v) => ColumnValue::Int(*v),
            PropValue::UInt(v) => ColumnValue::UInt(*v),
            PropValue::Long(v) => ColumnValue::Long(*v),
            PropValue::ULong(v) => ColumnValue::ULong(*v),
            PropValue::Float(v) => ColumnValue::Float(*v),
            PropValue::Double(v) => ColumnValue::Double(*v),
            PropValue::String(v) => ColumnValue::String(v),
            PropValue::Json(v) => ColumnValue::Json(v),
            PropValue::DateTime(v) => ColumnValue::DateTime(v),
            PropValue::Binary(v) => ColumnValue::Binary(v),
        }
    }
}

/// One decoded property: (column index, value).
pub type PropRow = Vec<(u16, PropValue)>;

/// Decode the properties buffer of a feature against the header's columns.
pub fn decode_properties(header: &Header, feature: &Feature) -> Result<PropRow> {
    let mut row = Vec::new();
    let Some(columns) = header.columns() else {
        return Ok(row);
    };
    let Some(properties) = feature.properties() else {
        return Ok(row);
    };
    let bytes = properties.bytes();
    let mut offset = 0usize;

    let err = || Error::Format("malformed properties buffer".into());

    while offset + 2 <= bytes.len() {
        let col_idx = u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap());
        offset += 2;
        if col_idx as usize >= columns.len() {
            return Err(err());
        }
        let col = columns.get(col_idx as usize);
        macro_rules! fixed {
            ($t:ty, $variant:ident) => {{
                const N: usize = std::mem::size_of::<$t>();
                if offset + N > bytes.len() {
                    return Err(err());
                }
                let v = <$t>::from_le_bytes(bytes[offset..offset + N].try_into().unwrap());
                offset += N;
                PropValue::$variant(v)
            }};
        }
        let value = match col.type_() {
            ColumnType::Bool => {
                if offset >= bytes.len() {
                    return Err(err());
                }
                let v = bytes[offset] != 0;
                offset += 1;
                PropValue::Bool(v)
            }
            ColumnType::Byte => fixed!(i8, Byte),
            ColumnType::UByte => fixed!(u8, UByte),
            ColumnType::Short => fixed!(i16, Short),
            ColumnType::UShort => fixed!(u16, UShort),
            ColumnType::Int => fixed!(i32, Int),
            ColumnType::UInt => fixed!(u32, UInt),
            ColumnType::Long => fixed!(i64, Long),
            ColumnType::ULong => fixed!(u64, ULong),
            ColumnType::Float => fixed!(f32, Float),
            ColumnType::Double => fixed!(f64, Double),
            ColumnType::String | ColumnType::Json | ColumnType::DateTime => {
                if offset + 4 > bytes.len() {
                    return Err(err());
                }
                let len =
                    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
                offset += 4;
                if offset + len > bytes.len() {
                    return Err(err());
                }
                let s = std::str::from_utf8(&bytes[offset..offset + len])
                    .map_err(|_| Error::Format("invalid utf8 in property".into()))?
                    .to_string();
                offset += len;
                match col.type_() {
                    ColumnType::Json => PropValue::Json(s),
                    ColumnType::DateTime => PropValue::DateTime(s),
                    _ => PropValue::String(s),
                }
            }
            ColumnType::Binary => {
                if offset + 4 > bytes.len() {
                    return Err(err());
                }
                let len =
                    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
                offset += 4;
                if offset + len > bytes.len() {
                    return Err(err());
                }
                let v = bytes[offset..offset + len].to_vec();
                offset += len;
                PropValue::Binary(v)
            }
            other => {
                return Err(Error::Format(format!("unsupported column type {other:?}")));
            }
        };
        row.push((col_idx, value));
    }
    Ok(row)
}

/// Column names of a header, in column-index order.
pub fn column_names(header: &Header) -> Vec<(String, ColumnType)> {
    header
        .columns()
        .map(|cols| {
            cols.iter()
                .map(|c| (c.name().to_string(), c.type_()))
                .collect()
        })
        .unwrap_or_default()
}

//! Low-level access to a FlatGeobuf "section": a complete fgb byte range
//! (magic + header + optional index + data) embedded at an arbitrary offset
//! of a larger file. Used both for the FGBO body and for overview/segments
//! sections (Profile A reuses fgb encodings wholesale).
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
        let mut win = WindowReader::new(r, stats, self.index_offset, self.index_size);
        let mut items = PackedRTree::stream_search(
            &mut win,
            self.features_count as usize,
            self.node_size,
            min_x,
            min_y,
            max_x,
            max_y,
        )?;
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
        if len > 512 * 1024 * 1024 {
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
}

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

//! Shared helpers: open an FGBO reader from a local path or an http(s) URL.

use anyhow::{Context, Result};
use fgbo::FgboReader;
use std::fs::File;
use std::io::{BufReader, Read, Seek};

/// Object-safe Read + Seek for "file or HTTP" readers.
pub trait ReadSeek: Read + Seek + Send {}
impl<T: Read + Seek + Send> ReadSeek for T {}

pub type AnyReader = FgboReader<Box<dyn ReadSeek>>;

pub fn is_url(spec: &str) -> bool {
    spec.starts_with("http://") || spec.starts_with("https://")
}

/// Open `spec` as an FGBO (or plain fgb) reader: local file path or URL.
pub fn open_reader(spec: &str) -> Result<AnyReader> {
    let inner: Box<dyn ReadSeek> = if is_url(spec) {
        Box::new(fgbo::HttpRangeReader::open(spec)?)
    } else {
        Box::new(BufReader::new(
            File::open(spec).with_context(|| format!("cannot open {spec}"))?,
        ))
    };
    Ok(FgboReader::open(inner)?)
}

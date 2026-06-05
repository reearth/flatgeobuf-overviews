//! Browser/worker bindings for the FGBO reader.
//!
//! The browser cannot block on I/O, so reading is retry-driven:
//!
//! ```js
//! const f = new Fgbo(fileLength);
//! let mvt;
//! while ((mvt = f.tile(z, x, y)) === undefined) {
//!   const [offset, len] = f.needed();          // first missing range
//!   const bytes = await fetchRange(url, offset, len); // over-fetch ok
//!   f.feed(offset, bytes);
//! }
//! // mvt: Uint8Array of MVT protobuf bytes
//! ```
//!
//! Every retry makes strict progress (footer → directory → section
//! header → index nodes → feature batches), and fed ranges are cached,
//! so warm tiles complete without any fetching.

use fgbo::sparse::{ChunkCache, MissingRange, SparseReader};
use fgbo::{render_tile, FgboReader, TileOptions};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct Fgbo {
    len: u64,
    cache: ChunkCache,
    missing: MissingRange,
    reader: Option<FgboReader<SparseReader>>,
}

#[wasm_bindgen]
impl Fgbo {
    /// `file_len` is the total byte length of the remote FGBO file
    /// (e.g. from a HEAD request or `Content-Range`).
    #[wasm_bindgen(constructor)]
    pub fn new(file_len: f64) -> Fgbo {
        Fgbo {
            len: file_len as u64,
            cache: ChunkCache::new(),
            missing: MissingRange::new(),
            reader: None,
        }
    }

    /// Insert fetched bytes at `offset` (over-fetching beyond the
    /// requested range is encouraged — everything is cached).
    pub fn feed(&mut self, offset: f64, bytes: &[u8]) {
        self.cache.insert(offset as u64, bytes.to_vec());
    }

    /// The first missing `[offset, len]` recorded by the last failed
    /// operation, if any.
    pub fn needed(&self) -> Option<Vec<f64>> {
        // peek without consuming: take + restore is fine single-threaded,
        // but simpler to consume — tile()/open retries re-record misses
        self.missing.take().map(|(o, l)| vec![o as f64, l as f64])
    }

    /// Bytes currently cached (diagnostics).
    pub fn cached_bytes(&self) -> f64 {
        self.cache.cached_bytes() as f64
    }

    fn ensure_open(&mut self) -> Result<bool, JsValue> {
        if self.reader.is_some() {
            return Ok(true);
        }
        let r = SparseReader::new(self.cache.clone(), self.missing.clone(), self.len);
        match FgboReader::open(r) {
            Ok(reader) => {
                self.reader = Some(reader);
                Ok(true)
            }
            Err(e) => {
                if self.missing_pending() {
                    Ok(false) // caller should fetch + retry
                } else {
                    Err(JsValue::from_str(&format!("open failed: {e}")))
                }
            }
        }
    }

    fn missing_pending(&self) -> bool {
        self.missing.peek().is_some()
    }

    /// True once the footer/directory/body header are parsed.
    pub fn is_open(&self) -> bool {
        self.reader.is_some()
    }

    /// True when the file has an FGBO directory (vs plain fgb).
    pub fn is_fgbo(&mut self) -> Result<Option<bool>, JsValue> {
        if !self.ensure_open()? {
            return Ok(None);
        }
        Ok(Some(self.reader.as_ref().unwrap().is_fgbo()))
    }

    /// Layer name from the body header (None = needs more data).
    pub fn layer_name(&mut self) -> Result<Option<String>, JsValue> {
        if !self.ensure_open()? {
            return Ok(None);
        }
        Ok(Some(self.reader.as_ref().unwrap().layer_name()))
    }

    /// Render tile z/x/y. `undefined` means "fetch `needed()` and call
    /// again"; a `Uint8Array` is the finished MVT.
    pub fn tile(&mut self, z: u8, x: u32, y: u32) -> Result<Option<Vec<u8>>, JsValue> {
        if !self.ensure_open()? {
            return Ok(None);
        }
        let reader = self.reader.as_mut().unwrap();
        match render_tile(reader, z, x, y, &TileOptions::default()) {
            Ok(t) => Ok(Some(t.data)),
            Err(e) => {
                if self.missing_pending() {
                    Ok(None)
                } else {
                    Err(JsValue::from_str(&format!("tile {z}/{x}/{y} failed: {e}")))
                }
            }
        }
    }
}

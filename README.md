# FlatGeobuf Overviews (FGBO)

Reference implementation (Rust) of a scale-optimized, FlatGeobuf-compatible
format.

FGBO appends three kinds of sections — an **importance sidecar**,
**overview levels**, and **segments** — after the data section of a
FlatGeobuf (fgb) file, accelerating on-demand tile rendering at any zoom
while keeping full compatibility with existing fgb readers. It is the
vector analogue of what COG did for rasters and COPC did for point clouds.

- Design document: [IDEA.md](IDEA.md)
- Binary specification (v0): [SPEC.md](SPEC.md)
- Compatibility verification report: [COMPAT.md](COMPAT.md)

```
┌─────────────────────────────────────────────┐
│ plain FlatGeobuf (full resolution)          │ ← readable by any fgb reader
├─────────────────────────────────────────────┤
│ importance sidecar (per-vertex tolerance)   │
│ overview levels (pre-simplified mini-fgb)   │ ← only FGBO-aware readers
│ segments (pre-clipped large features)       │    get faster
│ directory + footer (32B discovery at EOF)   │
└─────────────────────────────────────────────┘
```

## Workspace layout

| crate | contents |
|---|---|
| [`crates/fgbo`](crates/fgbo) | library: encoder, decoder, MVT tile generation |
| [`crates/fgbo-cli`](crates/fgbo-cli) | `fgbo` CLI: build / info / tile / serve / convert |

## Usage

```sh
# Build an FGBO file from an fgb (input must have index + feature count)
cargo run -rp fgbo-cli -- build input.fgb -o output.fgb

# Convert GeoJSON or index-less fgb first
cargo run -rp fgbo-cli -- convert input.geojson -o input.fgb

# Inspect
cargo run -rp fgbo-cli -- info output.fgb

# Render one MVT tile (--stats prints range-read counts/bytes)
cargo run -rp fgbo-cli -- tile output.fgb 2 3 1 --stats -o tile.mvt

# Tile server + MapLibre debug page (http://127.0.0.1:8080/)
# /compare shows fgb vs FGBO side by side with live I/O + timing stats
cargo run -rp fgbo-cli -- serve output.fgb
```

### Side-by-side comparison demo

```sh
./examples/compare/run.sh   # downloads NE 10m countries, builds, serves
# open http://127.0.0.1:8080/compare
```

### Benchmark (synthetic buildings)

```sh
./examples/bench/run.sh     # 1M synthetic buildings, baseline vs FGBO
```

With 1M building footprints (+17% storage, **1.7 s build** — ~40× faster
than baking the same data to PMTiles): **~10–12× faster tiles and ~12×
less I/O at z10–11** via overviews, parity at z12+ where bbox queries are
already selective. Numbers and caveats in
[examples/README.md](examples/README.md).

Two synced MapLibre maps render the same file through the plain-fgb
baseline path (left) and the FGBO path (right), with per-tile generation
time, bytes read, and range-read counts. With NE 10m countries: ~2× less
I/O and ~3× faster tiles at low zoom (overviews), ~5× less I/O and ~6×
faster over large coastline features at z13 (segments). See
[examples/README.md](examples/README.md).

As a library:

```rust
use fgbo::{encode_file, render_tile, EncodeOptions, FgboReader, TileOptions};

// Encode
let report = encode_file(&input, &output, &EncodeOptions::default())?;

// Tile generation (works on FGBO and plain fgb; plain fgb falls back to
// on-the-fly simplification)
let mut reader = FgboReader::open_file(&output)?;
let tile = render_tile(&mut reader, 2, 3, 1, &TileOptions::default())?;
// tile.data = MVT bytes / reader.stats = range-read counts and bytes
```

## How it works

- **importance**: at build time a modified Douglas–Peucker runs once,
  assigning each vertex the largest tolerance it survives, as a u16
  (geojson-vt's per-vertex significance externalized to a file).
  Per-request simplification becomes an O(n) filter.
- **overviews**: per zoom band (default z0–4 / z5–8 / z9–11), the
  importance-filtered and small-feature-thinned feature set is stored as a
  mini-fgb with its own Hilbert R-tree. Low-zoom tile requests read only
  that section (solving low-zoom I/O).
- **segments**: features above a vertex threshold (admin boundaries,
  rivers, …) are pre-clipped at the z12 grid. High zooms no longer read
  500k vertices for one tile.
- **deterministic builds**: identical input → byte-identical output
  (CDN caching, diff distribution, verification).

## Verification status

- Unit + integration tests: `cargo test` (format roundtrips, importance
  monotonicity, clipping, all three read paths, determinism, plain-fgb
  compatibility)
- Compatibility measurements (Rust 6.0.1 / JS 4.4.0 / GDAL 3.12.0):
  [COMPAT.md](COMPAT.md)
  - Index-driven paths (bbox / HTTP range) compatible in every
    implementation tested ✅
  - Only the JS full-scan path "yields all features, then errors"
    (the sentinel prevents misreads; upstream patch planned) ⚠️

## Known v0 limitations / TODO

- Z/M values are treated as 2D (importance uses 2D distances)
- Overview attributes are fully copied (attribute elision / ID-reference
  modes not yet implemented)
- Remote reading (HTTP range reader) and wasm build not yet implemented
  (`FgboReader` is built on a `Read + Seek` abstraction, so the extension
  is straightforward)
- No range-read coalescing yet — read `--stats` request counts as an upper
  bound for an HTTP deployment
- Benchmarks on real data (PLATEAU / OSM extracts) are future work; see
  [examples/README.md](examples/README.md) for synthetic-building numbers

# Examples

## compare — fgb vs FGBO overview rendering, side by side

A MapLibre page with two synced maps rendering tiles from **the same FGBO
file through two read paths**:

- **left / orange — plain fgb (baseline)**: body bbox query + per-request
  Douglas–Peucker, i.e. what a tiler can do with a plain FlatGeobuf
- **right / green — FGBO**: overview sections at low zoom, importance
  filter at high zoom, segments for oversized features

Each pane shows live counters fed by per-tile response headers: tile
generation time, bytes read from the file (≒ HTTP range-request bytes),
range-read count, and which FGBO path served the tile
(`Overview(n)` / `BodyImportance` / `BodyLive`).

```sh
./examples/compare/run.sh   # downloads NE 10m countries, builds, serves
# then open http://127.0.0.1:8080/compare
```

Zoom out to z0–z4 and pan around: the baseline pane reads (and simplifies)
nearly the entire file for every low-zoom tile, while the FGBO pane reads
only the small overview section. Use "reset counters" to start a clean
measurement at any viewpoint.

To try another dataset, point the server at any FGBO file:

```sh
cargo run --release -p fgbo-cli -- build your.fgb -o your.o.fgb
cargo run --release -p fgbo-cli -- serve your.o.fgb
```

The demo data uses Natural Earth 10m countries (~550k vertices) because
overview gains scale with source resolution; the 110m file in `testdata/`
is already generalized and shows little difference.

## bench — synthetic buildings, baseline vs FGBO by zoom band

World-polygon datasets understate FGBO: the low-zoom problem really bites
when a file holds hundreds of thousands to millions of small features.
`fgbo synth` generates a deterministic building-footprint dataset (city
clusters, 90% houses / 9% mid / 1% large), and `fgbo bench` renders the
same tiles through both read paths:

```sh
./examples/bench/run.sh           # 1,000,000 buildings (default)
./examples/bench/run.sh 200000    # smaller variant
```

Measured on an Apple-silicon laptop, 1,000,000 buildings
(body 210 MiB, FGBO total 246 MiB — **+17.1%**; overviews z0–8 are empty
by design, z9–11 keeps the 84k buildings larger than ~1 px):

| z | baseline ms | FGBO ms | speedup | baseline read | FGBO read | I/O ratio | feats base | feats FGBO | FGBO source |
|---|---|---|---|---|---|---|---|---|---|
| 8 | 887.3 | 0.0 | – | 24.5 MiB | 0 B | – | 8563 | 0 | Overview(1) |
| 10 | 876.2 | 74.7 | 11.7x | 22.5 MiB | 1.9 MiB | 11.9x | 96856 | 8956 | Overview(2) |
| 11 | 247.8 | 23.6 | 10.5x | 6.2 MiB | 538.2 KiB | 11.7x | 29022 | 2412 | Overview(2) |
| 12 | 141.3 | 126.9 | 1.1x | 3.3 MiB | 3.3 MiB | 1.0x | 15416 | 15417 | BodyImportance |
| 14 | 14.7 | 14.0 | 1.1x | 337.9 KiB | 337.9 KiB | 1.0x | 1469 | 1469 | BodyImportance |

(medians over 8 density-weighted sampled tiles per zoom; "read" = bytes
range-read from the file per tile)

How to read this honestly:

- **z8**: the baseline spends ~0.9 s and 24.5 MiB per tile rendering 8.5k
  buildings that are invisible at that scale; FGBO returns an empty tile
  instantly because build-time thinning encoded the "buildings don't render
  here" decision. This is the architectural point, not a like-for-like
  rendering.
- **z10–11**: the like-for-like win — overviews serve the ~1-px-and-larger
  subset: **~10–12× faster, ~12× less I/O**.
- **z12–14**: bbox queries are already selective, and building footprints
  have so few vertices that simplification is irrelevant — FGBO is at
  parity (the reader skips importance reads for low-vertex features, so
  the sidecar costs nothing here).
- Storage cost for all of this: **+17%** over the plain fgb.

The 200k variant shows the same shape (~10–12× at z10–11, parity at
z12+), scaled down: baseline z10 tiles cost ~175 ms instead of ~880 ms.

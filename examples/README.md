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

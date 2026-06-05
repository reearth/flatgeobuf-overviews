# FlatGeobuf Overviews (FGBO) — Design Document

A scale-optimized, FlatGeobuf-compatible format.

Status: Draft v0 / reference implementation in this repository
Binary specification: [SPEC.md](SPEC.md) · Compatibility verification: [COMPAT.md](COMPAT.md)

---

## 0. Abstract

FlatGeobuf (fgb) is already a "cloud-optimized" vector format, designed around
partial reads via HTTP range requests. But fgb holds a single resolution, so
low-zoom tile rendering pays a double cost: (1) almost every feature
intersects the tile bbox, so I/O spans nearly the whole file, and (2) every
vertex read must then be simplified per request.

FGBO appends three kinds of sections after the fgb data section — an
**importance sidecar**, **overview levels**, and **segments** — keeping full
backward compatibility with existing fgb readers while making
on-demand tile rendering fast at any zoom. It is the vector analogue of what
Cloud-Optimized GeoTIFF did for rasters with overviews, and what COPC did for
point clouds with its octree VLR.

The core of the design is to serialize the output of a deterministic
`prepare` step (a pure function) — per-vertex importance, low-zoom
overviews, and cell-clipped fragments of oversized features — into a
cloud-optimized static file layout. FGBO fills the design space between
fgb's "features stay features" flexibility and PMTiles' low-zoom performance.

---

## 1. Background: decomposing fgb's low-zoom problem

fgb carries a packed Hilbert R-tree, so a bbox query fetches only the byte
ranges it needs. At high and mid zooms (small tile bboxes) this works
perfectly. The problem is low zoom, and the cost decomposes into two
distinct parts:

**I/O cost.** The shallower the zoom, the larger the tile bbox; near z0
almost all features intersect. A spatial index is a *narrowing* device, and
when there is nothing to narrow it is powerless: range requests span nearly
the entire file. Serving one z2 tile from a multi-GB fgb means reading
gigabytes.

**CPU cost.** Every vertex read must be simplified to the tile resolution
(Douglas–Peucker etc.) and features thinned, per request.

Crucially, **the two costs require different remedies**. CPU cost is solved
by precomputing simplification (importance), but I/O cost can only be solved
by *where the data lives* — layout. Storing importance alone solves half the
problem; the heart of low zoom is I/O, and the answer is overviews.

---

## 2. Prior art and lessons

### 2.1 COG — Cloud-Optimized GeoTIFF

A plain GeoTIFF constrained by convention: (a) tiled internal layout,
(b) reduced-resolution overview images, (c) IFD placement walkable by range
requests. Defining it as a **layout convention inside an existing format**
rather than a new format was decisive for adoption. To a non-COG reader it
is just a GeoTIFF.

Lesson: never give up compatibility. Make the value asymmetric — "only
aware readers get faster".

### 2.2 COPC — Cloud-Optimized Point Cloud

A valid LAZ 1.4 file with an octree hierarchy embedded in a VLR (Hobu,
2021). Readers that don't understand COPC ignore the VLR and read all
points as normal LAZ. Readers that do understand the octree fetch only the
chunks needed for a spatial extent and LOD. Strip the VLR and you are back
to standard LAZ.

Lesson: put extensions in the host format's officially ignorable areas.
Storing an LOD hierarchy (upper octree nodes = thinned point sets) inside
the same file is the direct precedent for FGBO's overviews.

### 2.3 PMTiles

A baked tile pyramid in a single file, with a directory walked by range
requests (Protomaps). Low-zoom performance is optimal, but once data is
baked into tiles, (a) feature-level editing, attribute joins, and schema
changes become impossible, and (b) style/layer changes require re-baking.
FGBO is complementary: it aims for "near-tile low-zoom performance without
baking tiles".

### 2.4 geojson-vt

Mapbox's client-side tiling library. At index time it runs a modified
Douglas–Peucker once, assigning each vertex the simplification tolerance it
survives; tile generation is then an O(n) filter. FGBO's importance sidecar
externalizes this per-vertex significance from an in-memory structure into a
portable file section.

### 2.5 The vario-scale research lineage

"Extract any-scale representations from a single data structure" has a
30-year research lineage led by van Oosterom et al.:

- **BLG-tree** (Binary Line Generalization tree): polyline vertices
  organized in an importance-ordered binary tree; simplification at any
  tolerance is a partial tree walk. The prototype of per-vertex importance.
- **tGAP** (topological Generalized Area Partitioning, 2005): records the
  process of merging least-important faces into neighbors as a tree,
  deriving area partitions at any LOD.
- **Space-Scale Cube** (van Oosterom & Meijers, IJGIS 2014): scale as a
  third dimension; 2D map scale change as a continuous 3D structure.
- **Progressive transfer** (Haunert, Dilo & van Oosterom, 2009): tGAP
  applied to coarse-to-fine progressive data transfer.

These are theoretically powerful but have hardly landed on web-delivery
infrastructure (range-request file layouts, host-format compatibility, edge
execution). FGBO is positioned as **implementation research: landing
vario-scale ideas on a cloud-optimized file layout**. Choosing "an
importance array + a few discrete overview levels" instead of a full
hierarchy like BLG-tree is a deliberate engineering trade-off against range
request granularity, implementation cost, and ecosystem compatibility.

### 2.6 From fgb's own design

fgb is four sections: magic bytes (8 B, 4th byte = major version), a
length-prefixed flatbuffer Header, an optional packed Hilbert R-tree Index,
and Data (length-prefixed Features). It is write-once by design — appending
features would invalidate the index. FGBO does not append features; it
places **independent sections after the Data section**, so the existing
index remains fully valid.

---

## 3. Design principles

1. **Valid fgb invariant** — an FGBO file is always a valid fgb file.
   Readers that only understand fgb read all full-resolution features
   (COPC's "still a valid LAZ" principle).
2. **Footer discovery** — extensions are discovered via a fixed-size footer
   at EOF (same shape as Parquet / PMTiles directories). Streaming readers
   that scan from the front are unaffected.
3. **Reuse fgb encodings** — feature representation and spatial indexing in
   the added sections reuse fgb's Feature flatbuffers and packed Hilbert
   R-tree wholesale. The only new encodings are the importance array and the
   directory. Implementations can call existing fgb library code.
4. **Deterministic build** — identical input produces byte-identical output
   (a direct consequence of `prepare` being a deterministic pure function).
   Compatible with CDN caching, diff distribution, and reproducibility
   verification.
5. **Immutable** — write-once like fgb. Editing workflows belong to a
   DB-backed read-through serving mode (§7.2); FGBO is the "distribute and
   host" mode.

---

## 4. Design overview (as implemented)

Exact byte layouts are specified in [SPEC.md](SPEC.md).

```
┌─────────────────────────────────────────────┐
│ magic bytes (8B)                            │ ─┐
│ Header (length-prefixed flatbuffer)         │  │ fully valid
│ Index (packed Hilbert R-tree)               │  │ plain fgb
│ Data (Feature records, full resolution)     │ ─┘
├─────────────────────────────────────────────┤
│ sentinel (4B, 0xFFFFFFFF)                   │ ─┐
│ section: importance sidecar                 │  │
│ section: overview level 0..n (mini-fgb)     │  │ FGBO extension area
│ section: segments (mini-fgb of fragments)   │  │ (fgb readers never
│ Directory (section offsets/sizes/meta)      │  │  reach it)
│ Footer (fixed 32B: magic + dir ref + CRC)   │ ─┘
└─────────────────────────────────────────────┘
```

### 4.1 Footer and directory

A reader range-reads the last 32 bytes once. If the magic `"FGBOVRV1"`
matches, the file is FGBO and the directory (section offsets, sizes, zoom
ranges, tolerances, build info) is fetched; otherwise it is treated as plain
fgb. The extra cost for HTTP delivery is one small range read at EOF.

### 4.2 Sentinel

The first 4 bytes of the extension area are `FF FF FF FF`. A non-conforming
reader that ignores `features_count` and walks Features to EOF will read
this as a feature length prefix of ~4 GiB and fail immediately, instead of
silently misparsing trailer bytes as geometry. Compatibility testing showed
this case is real (the JS full-scan path) and that the sentinel works as
designed — see [COMPAT.md](COMPAT.md).

### 4.3 Importance sidecar

In the same order as the Data section (Hilbert order), a u16 array per
feature, 1:1 with its vertex buffer, prefixed by an offset table for random
access by feature ordinal.

- Value: the largest squared tolerance (unit Web Mercator) at which the
  vertex survives simplification, log-quantized to u16 (linear quantization
  lacks resolution near zero, where high-zoom tolerances live).
- Ring and part endpoints are pinned to `u16::MAX` (always kept).
- Usage: for zoom z with tolerance tol(z), `keep iff importance[i] ≥ q(tol²)`
  — an O(n) filter. Quantization is monotone, so simplification results nest
  across zoom levels. The body index gives the feature ordinal, so only the
  needed importance ranges have to be read.

### 4.4 Overview sections (solving low-zoom I/O)

Each level is a **complete embedded mini-fgb** (magic + header + index +
data) holding the feature set for a zoom band (default z0–4 / z5–8 / z9–11):

- vertices filtered by importance at the level's max-zoom tolerance,
- small features thinned (sub-pixel bbox drop, tippecanoe-style),
- Hilbert-sorted with its own packed R-tree.

A low-zoom tile request picks the level by zoom range from the directory and
performs a normal fgb read (index → range reads) **within that section
only**, never touching full-resolution Data. This is the core of "tile-like
low-zoom performance without baking tiles" — the same role as COG overviews
and COPC's upper octree nodes, with data staying as features.

Making each section a self-describing complete fgb (rather than bare
index + data) costs a few hundred bytes and lets any existing fgb reader
implementation be pointed at a section as-is.

### 4.5 Segments section (solving high-zoom large features)

Features whose vertex count exceeds a threshold v_max (admin boundaries,
rivers, contours) are pre-clipped at a fixed grid of zoom zbase (default
z12). The fragments are stored as a **complete mini-fgb** (with its own
Hilbert R-tree), and the directory records which body ordinals were
segmented.

- At z ≥ zbase, a tile request excludes segmented features from the body
  results and bbox-queries the segments section instead — no more reading
  500k vertices of a prefecture boundary for one tile.
- Artificial cut edges at cell boundaries are absorbed by tile-edge
  clipping: tile boundaries at z ≥ zbase are always subdivisions of zbase
  cell boundaries (nested power-of-two grids), so the absorption is
  structurally guaranteed. Below zbase, overviews serve instead.
- Small features never appear in segments; body + importance is cheap
  enough for them.

Spatial lookup reuses the fgb R-tree (bbox query over the fragments
mini-fgb) rather than a dedicated cell table — the same I/O behavior with
far less new encoding, per design principle 3.

### 4.6 Read protocol (tile request z/x/y)

```
1. Read footer 32B; if FGBO, fetch Directory (first request only, then cached)
2. Branch on z:
   - z within an overview level's zoom_range
       → overview index → range-read intersecting features → encode MVT
   - z above all overview levels
       → body index → range-read intersecting features + importance
       → O(n) filter → encode MVT
       → if z ≥ zbase: skip segmented features in the body, bbox-query the
         segments section and merge fragments
3. Clip to tile bounds (+ buffer) → MVT
```

Typical range-read budget: footer/directory (first request only) + partial
index reads + a handful of feature reads. Combined with CDN/edge caching the
goal is latency approaching pre-baked tile delivery, with data staying as
features.

---

## 5. Compatibility

### 5.1 Why compatibility holds

- fgb's Header carries `features_count`, and the Index stores byte offsets
  *within Data*. Index-driven reads (bbox queries) complete inside the Data
  section and structurally never reach the trailer.
- FGBO **requires** an explicit `features_count` (plain fgb allows
  unknown = 0; FGBO forbids it), so count-driven sequential readers also
  stop correctly.

### 5.2 Verified behavior

Measured against the Rust reference implementation (6.0.1), flatgeobuf JS
(4.4.0), and GDAL/OGR (3.12.0) — full results in [COMPAT.md](COMPAT.md):

- Index-driven reads (bbox / HTTP range): **compatible in every
  implementation tested**.
- Sequential full scans: Rust and GDAL respect `features_count` and stop
  cleanly. The JS full-scan path reads to EOF; it yields all features
  correctly, then errors on the sentinel (no silent misreads). An upstream
  patch making JS respect `featuresCount` is the spec-conformant fix and the
  first concrete standardization action.
- Re-saving with other tools (e.g. `ogr2ogr`) drops the trailer and returns
  a plain fgb — same known caveat as COPC, documented as such.

### 5.3 Extension and identification

Following COPC's precedent of keeping `.laz`, FGBO **keeps the `.fgb`
extension** and identifies via footer magic. The infix convention `*.o.fgb`
may be used where distinguishing files matters, but the file naming rule
stays `*.fgb`.

---

## 6. Naming and standardization path

- **Spec name: FlatGeobuf Overviews (FGBO).** fgb is already
  cloud-optimized, so "Cloud-Optimized FlatGeobuf" would be an inaccurate
  novelty claim. What this extension adds is **scale optimization** — in COG
  terms, not the range-read part but the overview (mipmap) part. The name
  points there.
- Propose as a low-profile extension to the fgb community (the path COPC
  took in the LAZ community). Even if the proposal does not land, FGBO
  works as an independent extension since files remain plain fgb.
- The reference implementation is this repository (Rust). A wasm build for
  browser/Workers reading is the planned demonstration vehicle — a working
  reader is what makes a format proposal persuasive, not the document.

---

## 7. Serving model and positioning

### 7.1 The file as "the image of prepare"

The underlying model: a tile is "source of truth + the image of a
deterministic prepare function"; precomputation is a pure performance
optimization (prefetch) decoupled from correctness. FGBO is exactly **that
image serialized into a portable file**.

- `fgbo build` (offline CLI / warm job): fgb → FGBO
- tile server / Workers: mount FGBO directly as a read-through backend
- Deterministic build: same input → byte-identical output, making
  distributed builds, verification, and caching trivial

### 7.2 Zero-DB delivery

Put an FGBO file on R2 / S3 / static hosting and a tile URL stands up with
Workers (or client wasm) + range requests. No database. Files are immutable,
so cache invalidation does not exist as a problem. When editing is needed,
move to a DB-backed read-through mode (PostGIS etc.):

| Mode | Source of truth | Edits | Serving cost |
|---|---|---|---|
| DB read-through | PostGIS etc. | immediate | DB + Workers |
| **FGBO static** | FGBO file | rebuild | storage + Workers only |

### 7.3 Positioning

| | FlatGeobuf | **FGBO** | PMTiles |
|---|---|---|---|
| Contents | features (single resolution) | features + importance + overviews + segments | baked tiles |
| Low-zoom I/O | nearly full read | overview section only | minimal |
| Simplification CPU | full per request | O(n) filter | none (baked) |
| Style/schema changes | free | free (still features) | re-bake |
| Dynamic attributes/filtering | yes | yes | no |
| Plain-fgb reader compat | — | full (reads as plain fgb) | none |
| Edits | rewrite | rebuild | re-bake |

FGBO sits between "feature flexibility" and "tile-grade delivery
performance".

---

## 8. Risks and mitigations

- **Yet another standard**: a new format proposal can end as "one more
  standard". Mitigations: full plain-fgb compatibility (existing assets
  keep their value), extension proposal to the fgb community, reference
  implementation first, and following the proven COG/COPC path.
- **Trailer-misreading readers**: confirmed real for the JS full-scan path
  (see [COMPAT.md](COMPAT.md)). Mitigated by the sentinel (immediate error,
  no misparse) and an upstream `featuresCount` patch proposal.
- **Overview storage overhead**: overviews and segments grow the file (COG
  overviews add roughly a third, structurally similar). Coarser levels
  shrink exponentially in vertices and features, so the increase is
  bounded; level count and attribute duplication will be build options.
  Note: on already-generalized data (e.g. Natural Earth 110m) overviews
  barely shrink relative to the body — the gains materialize on
  high-resolution sources.
- **Misreading immutability**: document clearly that "instant updates"
  belong to DB-backed serving and FGBO is for
  distribution/publishing/archives requiring rebuilds.

---

## 9. Roadmap

1. ~~**Compatibility measurements** (§5.2): trailer behavior across major
   fgb implementations~~ — done, see [COMPAT.md](COMPAT.md)
2. ~~**Format v0**: footer / directory / three section binary layouts~~ —
   done, see [SPEC.md](SPEC.md)
3. ~~**Reference implementation**: writer + reader + tiler + CLI + tile
   server~~ — done (this repository)
4. **Benchmarks**: real data (PLATEAU buildings, admin boundaries, OSM
   extracts), plain fgb vs FGBO by zoom band — range-read bytes and tile
   generation time at low zoom, memory at high zoom over large features
5. **Public demo**: FGBO on static hosting + browser wasm reader showing
   zero-DB tile delivery
6. **Spec publication & extension proposal**: spec + implementation +
   benchmarks, then approach the fgb community (including the JS
   `featuresCount` patch)

---

## 10. References

- FlatGeobuf specification / B. Harrtell — https://flatgeobuf.org / https://github.com/flatgeobuf/flatgeobuf
- H. Williams, "Kicking the Tires: Flatgeobuf" / "Flatgeobuf: Implementer's Guide" (2022) — https://worace.works/2022/02/23/kicking-the-tires-flatgeobuf/
- Cloud Optimized GeoTIFF — https://cogeo.org
- COPC: Cloud Optimized Point Cloud Specification 1.0 (Hobu, 2021) — https://copc.io
- PMTiles (Protomaps) — https://github.com/protomaps/PMTiles
- geojson-vt (Mapbox) — prior art for per-vertex simplification significance — https://github.com/mapbox/geojson-vt
- P. van Oosterom, "Variable-scale topological data structures suitable for progressive data transfer: the GAP-face tree and GAP-edge forest", CaGIS 32(4), 2005 — tGAP
- P. van Oosterom & M. Meijers, "Vario-scale data structures supporting smooth zoom and progressive transfer of 2D and 3D data", IJGIS 28(3), 2014 — Space-Scale Cube
- J.-H. Haunert, A. Dilo, P. van Oosterom, "Constrained set-up of the tGAP structure for progressive vector data transfer", Computers & Geosciences 35(11), 2009
- I. Kamel & C. Faloutsos, "Hilbert R-tree: An improved R-tree using fractals", VLDB 1994 — the packed Hilbert R-tree underlying fgb's index

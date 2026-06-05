# FGBO v0 Binary Specification

Status: Draft v0, kept in sync with the reference implementation.
Design rationale and background: [IDEA.md](IDEA.md).
All integers are little-endian.

## 1. File layout

```
┌─────────────────────────────────────────────┐
│ plain FlatGeobuf (magic/Header/Index/Data)  │  ← fully valid fgb
├─────────────────────────────────────────────┤
│ sentinel (4B = FF FF FF FF)                 │
│ section: importance sidecar                 │
│ section: overview level 0..n (mini-fgb)     │
│ section: segments (mini-fgb)                │
│ directory                                   │
│ footer (fixed 32B)                          │
└─────────────────────────────────────────────┘
```

Constraints:
- The fgb body MUST have `features_count > 0` and a packed R-tree index
  (both optional in plain fgb, required in FGBO).
- Write-once. No appends or partial updates (rebuild instead).
- Builds are deterministic: identical input and options produce
  byte-identical output.

### 1.1 Sentinel

The first 4 bytes of the extension area are fixed `FF FF FF FF`. A
non-conforming reader that ignores `features_count` and keeps reading
Features until EOF will interpret this as a feature length prefix of an
absurd size and fail immediately, preventing silent misreads.

## 2. Footer (fixed 32 bytes at EOF)

| bytes | content |
|---|---|
| 0..8 | magic `"FGBOVRV1"` |
| 8..16 | directory offset (u64) |
| 16..24 | directory size (u64) |
| 24..28 | CRC32 of the directory (u32) |
| 28..32 | reserved (0) |

Readers range-read the last 32 bytes once: magic match → FGBO; no match →
plain fgb.

## 3. Directory

```
u8   version (= 1)
u8   importance present (0/1)
       u64 offset, u64 size, u64 feature_count
u8   overview count
       each: u64 offset, u64 size, u8 min_zoom, u8 max_zoom,
             u16 tolerance_q, u64 feature_count
u8   segments present (0/1)
       u64 offset, u64 size, u8 zbase, u32 v_max, u64 fragment_count,
       u64 segmented_count, u64 × segmented_count
         (body ordinals of segmented features, ascending)
u16  build_info length + UTF-8 bytes
```

Overview entries are ordered by ascending `min_zoom` with mutually disjoint
zoom ranges.

## 4. Importance sidecar

In the **same order** as the body Data section (file order = Hilbert sort
order), one u16 array per feature, mapping 1:1 to its vertex sequence.

```
u64                   feature_count
u64 × (count + 1)     offsets (bytes relative to payload start;
                      the final entry is the total payload length)
u16 × total_vertices  payload
```

- Vertex order is fgb coordinate storage order (parts in order, exterior
  ring then interior rings, including the closing duplicate vertex).
- Value: the largest squared tolerance (unit Web Mercator space, computed in
  f64) at which the vertex survives simplification, log-quantized:
  - `q = clamp(1 + floor((log2(d²) + 64) / 64 × 65533), 1, 65534)`
  - `0` = reserved, `65535` = always kept (ring/part endpoints)
- Usage: a tile at zoom z keeps vertex i iff
  `importance[i] ≥ threshold_q(tol²(z))` — an O(n) filter, where
  `tol(z) = 1 / (2^z × extent)` (default extent 4096).
- Quantization is monotone, so simplification results nest between any two
  zoom levels.

## 5. Overview sections

Each level is a **complete mini-FlatGeobuf** (magic + Header + Index +
Data) — self-describing, so any existing fgb reader implementation can be
pointed at the section as-is, at a cost of a few hundred bytes per level.
Same schema and CRS as the body, containing the feature set produced by:

1. Vertex filter: `importance ≥ tolerance_q` (1 px equivalent at the
   level's max_zoom).
2. Feature thinning: drop lines/polygons whose mercator bbox is smaller
   than `drop_small_units / (2^max_zoom × extent)` in both dimensions
   (default 16 units = 1 screen pixel at extent 4096).
3. Drop degenerate results (collapsed rings etc.).

Attributes are fully copied in v0 (attribute elision / ID-reference modes
are future build options).

A reader picks the level whose `[min_zoom, max_zoom]` contains z and
performs a normal fgb read (index → range reads) within that section. The
full-resolution Data section is never touched.

## 6. Segments section

Fragments of features whose vertex count exceeds `v_max`, pre-clipped at
the zbase (default z12) grid cell boundaries. Stored as a **complete
mini-FlatGeobuf** with geometry type `Unknown` (clipping can change
geometry types) and its own Hilbert R-tree; fragments retain the original
feature's attributes. The directory's `segmented_ordinals` identifies the
affected body features.

Reading at z ≥ zbase:
1. Exclude features listed in `segmented_ordinals` from body bbox results.
2. Bbox-query the segments mini-fgb for intersecting fragments.

Spatial lookup deliberately reuses the fgb R-tree (bbox query) instead of a
dedicated Morton-cell table: equivalent I/O behavior with no new encoding,
per the "reuse fgb encodings" principle.

Tile boundaries at z ≥ zbase are subdivisions of zbase cell boundaries
(nested power-of-two grids), so artificial cut edges are absorbed by
tile-edge clipping. Below zbase, segments are not used (overviews cover
those zooms). Per-fragment importance is omitted in v0 (at z ≥ zbase the
required simplification is small).

## 7. Read protocol (tile z/x/y)

```
1. Read last 32B → FGBO detection → fetch directory
   (first request only, cache afterwards)
2. Branch on z:
   a. z within an overview level → index → range reads within that
      section only → MVT
   b. otherwise (z above the deepest overview):
      - body index → range-read intersecting features + importance
      - O(n) simplification filter
      - if z ≥ zbase: skip segmented features, bbox-query the segments
        section and merge fragments
3. Clip to tile bounds + buffer → MVT encode
```

## 8. Compatibility

- An FGBO file is always a valid fgb. Index-driven reads (bbox / HTTP
  range) structurally never reach the trailer (measured:
  [COMPAT.md](COMPAT.md)).
- Sequential readers that respect `features_count` (Rust implementation,
  GDAL) stop cleanly.
- Implementations that read to EOF (the flatgeobuf JS full-scan path) yield
  every feature, then fail fast on the sentinel — no misreads. An upstream
  patch to respect `featuresCount` will be proposed.
- Re-saving through other tools drops the trailer, returning a plain fgb
  (same caveat as COPC).
- The file extension stays `.fgb`; the infix convention `*.o.fgb` is
  optional.

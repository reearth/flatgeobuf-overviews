# FGBO Compatibility Verification Report

Measured results for the verification items in [IDEA.md](IDEA.md) §5.
**An FGBO file = valid fgb body + 4-byte sentinel + extension sections +
directory + 32-byte footer**; this report records how major fgb
implementations behave when reading one.

- Date: 2026-06-06
- Test file: `testdata/countries.o.fgb` (Natural Earth countries, 179
  features, produced by `fgbo build`, ~600 KiB trailer attached)
- Sentinel: `0xFFFFFFFF` at the start of the extension area (an invalid
  value that triggers an immediate error if read as a feature length
  prefix)

## Results matrix

| Implementation | Version | Read path | Respects features_count | Behavior on FGBO file |
|---|---|---|---|---|
| Rust `flatgeobuf` | 6.0.1 | `select_all` (seek) | ✅ | ✅ stops cleanly at 179 features |
| Rust `flatgeobuf` | 6.0.1 | `select_all_seq` (streaming) | ✅ | ✅ stops cleanly at 179 features |
| Rust `flatgeobuf` | 6.0.1 | `select_bbox` (index) | n/a | ✅ correct (index stays within Data) |
| JS `flatgeobuf` | 4.4.0 | `deserialize(bytes)` full scan | ❌ **reads to EOF** | ⚠️ yields all 179 features, then errors on the 180th (`TypeError: … partsLength`) |
| JS `flatgeobuf` | 4.4.0 | `deserialize(bytes, rect)` (index) | n/a | ✅ correct |
| JS `flatgeobuf` | 4.4.0 | `deserialize(stream)` streaming | ❌ **reads to EOF** | ⚠️ yields all 179 features, then errors (`Error: invalid length` — sentinel working) |
| JS `flatgeobuf` | 4.4.0 | `deserialize(url, rect)` HTTP range | n/a | ✅ correct |
| GDAL/OGR | 3.12.0 | `ogrinfo` full read | ✅ | ✅ Feature Count 179, no errors |
| GDAL/OGR | 3.12.0 | `-spat` bbox filter | n/a | ✅ correct |
| GDAL/OGR | 3.12.0 | `ogr2ogr` re-export | — | ✅ works (**trailer dropped**, as expected) |

Reproduction:

```sh
# Rust (covered by the integration test)
cargo test -p fgbo --test roundtrip

# JS
cd compat/js && npm install
node test.mjs ../../testdata/countries.fgb ../../testdata/countries.o.fgb

# GDAL
ogrinfo -ro -al -so testdata/countries.o.fgb
ogrinfo -ro testdata/countries.o.fgb countries -spat 135 30 145 45 -so
ogr2ogr -f FlatGeobuf /tmp/rt.fgb testdata/countries.o.fgb && tail -c 32 /tmp/rt.fgb | xxd
```

## Analysis

### 1. Cloud-optimized paths (bbox / HTTP range) are fully compatible everywhere ✅

The index → range-read path — fgb's reason to exist — only references byte
offsets within the Data section and structurally never reaches the trailer
(empirical confirmation of the compatibility argument in IDEA.md §5.1).
**FGBO's primary use cases (remote bbox queries, tile serving) hit no
issues at all.**

### 2. The JS full-scan path is the "reads to EOF" reader IDEA.md §5.2 predicted ⚠️

`deserialize(Uint8Array)` and the streaming path in flatgeobuf JS ignore
`featuresCount` and loop to the end of the buffer
(`generic/featurecollection.js`: `for (; p < u.capacity(); )`).

- **The sentinel works as designed**: all 179 features are yielded
  correctly, then an immediate error at the trailer head. No silent data
  corruption or misparsing occurs.
- However, generator consumers see "all features, then an exception", so
  code without `try/catch` treats the read as failed. **Full scans via the
  JS byte/stream paths must be documented as currently incompatible.**

Mitigations (to be stated in the spec):
1. **Upstream patch proposal**: stopping at `featuresCount > 0` is the
   spec-conformant behavior (the Rust implementation and GDAL already do
   this). Propose a PR to flatgeobuf/flatgeobuf.
2. Until then, JS consumers reading everything should pass a whole-world
   bbox to `deserialize(bytes, rect)` (the index path is safe), or treat
   the trailing error as EOF.

### 3. GDAL re-export drops the trailer, as expected (same as COPC) ✅

`ogr2ogr` output is a plain fgb (829 KiB → 206 KiB, footer magic gone).
COPC has the same known behavior ("re-saving in other tools drops the
extension"); documented as a spec caveat.

### 4. Not yet tested

- C++ (reference implementation), Java, and Go implementations. The
  Rust/JS/GDAL results establish that "does it respect features_count" is
  the deciding criterion.
- QGIS (uses GDAL, so presumed identical; not verified in the GUI).

## Conclusion

FGBO's compatibility design is empirically validated.

- Index-driven reads (fgb's core value): **compatible in every
  implementation tested**.
- Sequential full reads: compatible where `features_count` is respected
  (Rust, GDAL); EOF-readers (JS full scan) yield all features then error.
  The sentinel prevents misreads.
- The first concrete standardization action is the upstream
  `featuresCount` patch for JS.

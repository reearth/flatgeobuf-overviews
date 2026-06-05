// Compatibility test: flatgeobuf JS reading an FGBO file (fgb + trailer).
//
// Paths exercised:
//  1. deserialize(bytes)            — full scan (known to read to EOF)
//  2. deserialize(bytes, rect)      — in-memory bbox via index
//  3. deserializeStream(stream)     — streaming scan
//  4. deserializeFiltered(url,rect) — HTTP range reader
//
// Usage: node test.mjs <plain.fgb> <fgbo.fgb>

import { readFileSync } from "node:fs";
import { createServer } from "node:http";
import { stat, open } from "node:fs/promises";
// the single `deserialize` dispatches on input type:
// Uint8Array -> full scan / bbox, ReadableStream -> streaming, string -> HTTP
import { deserialize } from "flatgeobuf/lib/mjs/geojson.js";

const [plainPath, fgboPath] = process.argv.slice(2);
const results = [];

async function countIter(iter) {
  let n = 0;
  let error = null;
  try {
    for await (const _f of iter) n++;
  } catch (e) {
    error = e;
  }
  return { n, error };
}

function report(name, file, { n, error }) {
  const status = error ? `ERROR after ${n} features: ${String(error).slice(0, 120)}` : `OK (${n} features)`;
  results.push({ name, file, n, error: error ? String(error).slice(0, 200) : null });
  console.log(`${name.padEnd(34)} ${file.padEnd(10)} ${status}`);
}

const rect = { minX: 122, minY: 24, maxX: 154, maxY: 46 }; // around Japan

for (const [label, path] of [["plain", plainPath], ["fgbo", fgboPath]]) {
  const bytes = new Uint8Array(readFileSync(path));

  // 1. full scan
  report("deserialize(bytes)", label, await countIter(deserialize(bytes)));

  // 2. bbox via in-memory index
  report("deserialize(bytes, rect)", label, await countIter(deserialize(bytes, rect)));

  // 3. streaming
  const streamRes = await (async () => {
    const fh = await open(path);
    const stream = fh.readableWebStream();
    const r = await countIter(deserialize(stream));
    try { await fh.close(); } catch {}
    return r;
  })();
  report("deserializeStream(stream)", label, streamRes);
}

// 4. HTTP range reader (bbox) against a local range-supporting server
const server = createServer(async (req, res) => {
  const path = req.url.includes("fgbo") ? fgboPath : plainPath;
  const size = (await stat(path)).size;
  const range = req.headers.range;
  if (range) {
    const m = /bytes=(\d+)-(\d+)?/.exec(range);
    const start = Number(m[1]);
    const end = m[2] !== undefined ? Math.min(Number(m[2]), size - 1) : size - 1;
    const fh = await open(path);
    const buf = Buffer.alloc(end - start + 1);
    await fh.read(buf, 0, buf.length, start);
    await fh.close();
    res.writeHead(206, {
      "Content-Range": `bytes ${start}-${end}/${size}`,
      "Content-Length": buf.length,
      "Accept-Ranges": "bytes",
    });
    res.end(buf);
  } else {
    res.writeHead(200, { "Content-Length": size });
    res.end(readFileSync(path));
  }
});
await new Promise((r) => server.listen(0, r));
const port = server.address().port;

for (const label of ["plain", "fgbo"]) {
  report(
    "deserializeFiltered(url, rect)",
    label,
    await countIter(deserialize(`http://127.0.0.1:${port}/${label}.fgb`, rect))
  );
}
server.close();

console.log("\nJSON:" + JSON.stringify(results));

// Static file server with Range support — stands in for R2/S3/any static
// hosting. Serves the examples/wasm directory.
import { createServer } from "node:http";
import { stat, open, readFile } from "node:fs/promises";
import { join, normalize, extname } from "node:path";

const root = new URL(".", import.meta.url).pathname;
const port = Number(process.argv[2] || 8090);
const types = {
  ".html": "text/html", ".js": "text/javascript", ".wasm": "application/wasm",
  ".fgb": "application/octet-stream", ".json": "application/json",
};

createServer(async (req, res) => {
  try {
    let path = normalize(decodeURIComponent(new URL(req.url, "http://x").pathname));
    if (path === "/") path = "/index.html";
    const file = join(root, path);
    if (!file.startsWith(root)) throw new Error("forbidden");
    const size = (await stat(file)).size;
    const type = types[extname(file)] || "application/octet-stream";
    const m = /bytes=(\d+)-(\d+)?/.exec(req.headers.range || "");
    if (m) {
      const start = Number(m[1]);
      const end = m[2] !== undefined ? Math.min(Number(m[2]), size - 1) : size - 1;
      const fh = await open(file);
      const buf = Buffer.alloc(end - start + 1);
      await fh.read(buf, 0, buf.length, start);
      await fh.close();
      res.writeHead(206, {
        "Content-Range": `bytes ${start}-${end}/${size}`,
        "Content-Length": buf.length,
        "Content-Type": type,
        "Accept-Ranges": "bytes",
      });
      res.end(buf);
    } else {
      res.writeHead(200, {
        "Content-Length": size,
        "Content-Type": type,
        "Accept-Ranges": "bytes",
      });
      res.end(await readFile(file));
    }
  } catch (e) {
    res.writeHead(404);
    res.end(String(e));
  }
}).listen(port, () => console.log(`http://127.0.0.1:${port}/`));

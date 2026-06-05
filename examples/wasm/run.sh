#!/usr/bin/env bash
# Build the wasm reader, prepare demo data, and serve the static demo.
# The "server" is just static files + Range support — the FGBO decoding
# and MVT rendering happen in the browser.
set -euo pipefail
cd "$(dirname "$0")/../.."

if ! command -v wasm-pack >/dev/null; then
  echo "wasm-pack is required: cargo install wasm-pack" >&2
  exit 1
fi

wasm-pack build crates/fgbo-wasm --target web --release \
  --out-dir ../../examples/wasm/pkg

mkdir -p examples/wasm/data
if [ ! -f examples/wasm/data/demo.o.fgb ]; then
  echo "generating demo data (200k synthetic buildings)..."
  cargo run --release -p fgbo-cli -- synth -o examples/wasm/data/demo.fgb --features 200000
  cargo run --release -p fgbo-cli -- build examples/wasm/data/demo.fgb -o examples/wasm/data/demo.o.fgb
fi

echo "open http://127.0.0.1:8090/"
exec node examples/wasm/serve.mjs 8090

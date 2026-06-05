#!/usr/bin/env bash
# Synthetic-building benchmark: plain fgb baseline vs FGBO, per zoom band.
#
# Buildings are where fgb's low-zoom problem actually bites: hundreds of
# thousands to millions of small polygons, not a handful of world-sized
# ones. Usage:
#
#   ./examples/bench/run.sh [N_FEATURES]   # default 1,000,000
set -euo pipefail

cd "$(dirname "$0")/../.."
N="${1:-1000000}"
mkdir -p examples/bench/data
FGB="examples/bench/data/bldg_${N}.fgb"
FGBO="examples/bench/data/bldg_${N}.o.fgb"

cargo build --release -p fgbo-cli

if [ ! -f "$FGBO" ]; then
  echo "generating $N synthetic buildings..."
  ./target/release/fgbo synth -o "$FGB" --features "$N"
  ./target/release/fgbo build "$FGB" -o "$FGBO"
fi

./target/release/fgbo bench "$FGBO" --zooms 8,10,11,12,14 --tiles 8

echo
echo "(view it: ./target/release/fgbo serve $FGBO  ->  http://127.0.0.1:8080/compare)"

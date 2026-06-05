#!/usr/bin/env bash
# Build the comparison demo dataset and start the tile server.
#
# Uses Natural Earth 10m countries (~550k vertices) — high-resolution
# enough that overviews actually pay off (the 110m test file is already
# generalized, so it shows little difference).
#
# Requirements: curl, ogr2ogr (GDAL), cargo
set -euo pipefail

cd "$(dirname "$0")"
mkdir -p data

NE_URL="https://naciscdn.org/naturalearth/10m/cultural/ne_10m_admin_0_countries.zip"

if [ ! -f data/ne10m.o.fgb ]; then
  if [ ! -f data/ne10m.fgb ]; then
    if [ ! -f data/ne_10m_admin_0_countries.zip ]; then
      echo "downloading Natural Earth 10m countries..."
      curl -fL --retry 3 -o data/ne_10m_admin_0_countries.zip "$NE_URL"
    fi
    echo "converting to FlatGeobuf..."
    ogr2ogr -f FlatGeobuf data/ne10m.fgb \
      "/vsizip/data/ne_10m_admin_0_countries.zip/ne_10m_admin_0_countries.shp" \
      -t_srs EPSG:4326 -nlt PROMOTE_TO_MULTI -nln countries10m \
      -select NAME,ISO_A3,CONTINENT
  fi
  echo "building FGBO..."
  cargo run --release -p fgbo-cli -- build data/ne10m.fgb -o data/ne10m.o.fgb
fi

cargo run --release -p fgbo-cli -- info data/ne10m.o.fgb
echo
echo "open http://127.0.0.1:8080/compare"
exec cargo run --release -p fgbo-cli -- serve data/ne10m.o.fgb

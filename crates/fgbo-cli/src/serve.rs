//! HTTP tile server: /tiles/{mode}/{z}/{x}/{y}.mvt + MapLibre debug pages.
//!
//! Two read modes serve the same file so they can be compared directly:
//! - `fgbo`     — the FGBO read protocol (overviews / importance / segments)
//! - `baseline` — plain-fgb behavior (body bbox query + on-the-fly DP)
//!
//! Per-tile statistics are exposed as response headers
//! (X-Tile-Source / X-Read-Bytes / X-Read-Requests / X-Gen-Ms) for the
//! /compare page.

use anyhow::Result;
use axum::extract::{Path as AxPath, State};
use axum::http::{header, HeaderName, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use fgbo::{render_tile, FgboReader, TileOptions};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

/// MapLibre comparison page (template; `__LAYER__` is substituted).
const COMPARE_HTML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../examples/compare/index.html"
));

struct AppState {
    file: PathBuf,
    layer: String,
    is_fgbo: bool,
}

pub fn run(file: PathBuf, addr: &str) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(file, addr))
}

async fn run_async(file: PathBuf, addr: &str) -> Result<()> {
    // open once for metadata + early validation
    let (layer, is_fgbo) = {
        let reader = FgboReader::open_file(&file)?;
        (reader.layer_name(), reader.is_fgbo())
    };
    tracing::info!(
        "serving {} (layer {layer:?}, FGBO: {is_fgbo}) on http://{addr}/ (comparison: http://{addr}/compare)",
        file.display()
    );

    let state = Arc::new(AppState {
        file,
        layer,
        is_fgbo,
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/compare", get(compare))
        .route("/tiles/{mode}/{z}/{x}/{y}", get(tile))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn tile(
    AxPath((mode, z, x, y)): AxPath<(String, u8, u32, String)>,
    State(state): State<Arc<AppState>>,
) -> Response {
    let baseline = match mode.as_str() {
        "fgbo" => false,
        "baseline" => true,
        _ => return (StatusCode::NOT_FOUND, "mode must be fgbo or baseline").into_response(),
    };
    // allow y, y.mvt, y.pbf
    let y = y
        .trim_end_matches(".mvt")
        .trim_end_matches(".pbf")
        .to_string();
    let Ok(y) = y.parse::<u32>() else {
        return (StatusCode::BAD_REQUEST, "invalid y").into_response();
    };
    if z > 24 || x >= (1u32 << z.min(24)) || y >= (1u32 << z.min(24)) {
        return (StatusCode::BAD_REQUEST, "tile out of range").into_response();
    }

    let file = state.file.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut reader = FgboReader::open_file(&file)?;
        let opts = TileOptions {
            baseline,
            ..Default::default()
        };
        let start = Instant::now();
        let tile = render_tile(&mut reader, z, x, y, &opts)?;
        let gen_ms = start.elapsed().as_secs_f64() * 1000.0;
        fgbo::Result::Ok((tile, gen_ms, reader.stats.bytes(), reader.stats.requests()))
    })
    .await;

    match result {
        Ok(Ok((tile, gen_ms, bytes, requests))) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/x-protobuf".to_string()),
                (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".to_string()),
                (header::CACHE_CONTROL, "no-store".to_string()),
                (
                    HeaderName::from_static("x-tile-source"),
                    format!("{:?}", tile.source),
                ),
                (HeaderName::from_static("x-read-bytes"), bytes.to_string()),
                (
                    HeaderName::from_static("x-read-requests"),
                    requests.to_string(),
                ),
                (HeaderName::from_static("x-gen-ms"), format!("{gen_ms:.1}")),
                (
                    HeaderName::from_static("x-feature-count"),
                    tile.feature_count.to_string(),
                ),
            ],
            tile.data,
        )
            .into_response(),
        Ok(Err(e)) => {
            tracing::error!("tile {mode}/{z}/{x}/{y}: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

async fn compare(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(COMPARE_HTML.replace("__LAYER__", &state.layer))
}

async fn index(State(state): State<Arc<AppState>>) -> Html<String> {
    let layer = &state.layer;
    let badge = if state.is_fgbo { "FGBO" } else { "plain fgb" };
    Html(format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>fgbo serve — {layer}</title>
<link rel="stylesheet" href="https://unpkg.com/maplibre-gl@4/dist/maplibre-gl.css">
<script src="https://unpkg.com/maplibre-gl@4/dist/maplibre-gl.js"></script>
<style>
  html, body, #map {{ margin: 0; height: 100%; }}
  #badge {{ position: absolute; top: 8px; left: 8px; z-index: 1;
    background: #222; color: #fff; padding: 4px 10px; border-radius: 4px;
    font: 12px/1.4 sans-serif; }}
  #badge a {{ color: #8cf; }}
</style>
</head>
<body>
<div id="badge">{layer} ({badge}) — <a href="/compare">side-by-side comparison</a></div>
<div id="map"></div>
<script>
const map = new maplibregl.Map({{
  container: 'map',
  style: {{
    version: 8,
    sources: {{
      fgbo: {{
        type: 'vector',
        tiles: [location.origin + '/tiles/fgbo/{{z}}/{{x}}/{{y}}.mvt'],
        minzoom: 0,
        maxzoom: 22
      }}
    }},
    layers: [
      {{ id: 'bg', type: 'background', paint: {{ 'background-color': '#10141a' }} }},
      {{ id: 'fill', type: 'fill', source: 'fgbo', 'source-layer': '{layer}',
        filter: ['==', ['geometry-type'], 'Polygon'],
        paint: {{ 'fill-color': '#3a86ff', 'fill-opacity': 0.35 }} }},
      {{ id: 'line', type: 'line', source: 'fgbo', 'source-layer': '{layer}',
        paint: {{ 'line-color': '#ffd166', 'line-width': 1.2 }} }},
      {{ id: 'pts', type: 'circle', source: 'fgbo', 'source-layer': '{layer}',
        filter: ['==', ['geometry-type'], 'Point'],
        paint: {{ 'circle-color': '#ef476f', 'circle-radius': 3 }} }}
    ]
  }},
  center: [136, 36],
  zoom: 4
}});
map.addControl(new maplibregl.NavigationControl());
</script>
</body>
</html>"#
    ))
}

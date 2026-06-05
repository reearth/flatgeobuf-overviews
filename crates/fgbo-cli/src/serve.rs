//! HTTP tile server: /tiles/{z}/{x}/{y}.mvt + MapLibre debug page.

use anyhow::Result;
use axum::extract::{Path as AxPath, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use fgbo::{render_tile, FgboReader, TileOptions};
use std::path::PathBuf;
use std::sync::Arc;

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
        "serving {} (layer {layer:?}, FGBO: {is_fgbo}) on http://{addr}/",
        file.display()
    );

    let state = Arc::new(AppState {
        file,
        layer,
        is_fgbo,
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/tiles/{z}/{x}/{y}", get(tile))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn tile(
    AxPath((z, x, y)): AxPath<(u8, u32, String)>,
    State(state): State<Arc<AppState>>,
) -> Response {
    // allow /tiles/z/x/y, /tiles/z/x/y.mvt, /tiles/z/x/y.pbf
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
        render_tile(&mut reader, z, x, y, &TileOptions::default())
    })
    .await;

    match result {
        Ok(Ok(tile)) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/x-protobuf"),
                (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
                (header::CACHE_CONTROL, "public, max-age=86400, immutable"),
            ],
            tile.data,
        )
            .into_response(),
        Ok(Err(e)) => {
            tracing::error!("tile {z}/{x}/{y}: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
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
</style>
</head>
<body>
<div id="badge">{layer} ({badge})</div>
<div id="map"></div>
<script>
const map = new maplibregl.Map({{
  container: 'map',
  style: {{
    version: 8,
    sources: {{
      fgbo: {{
        type: 'vector',
        tiles: [location.origin + '/tiles/{{z}}/{{x}}/{{y}}.mvt'],
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

pub(crate) mod index;
pub(crate) mod manifest;
pub(crate) mod models;
pub(crate) mod parser;
pub(crate) mod resolver;
pub(crate) mod scanner;
pub(crate) mod torrent;

use self::manifest::get_manifest;
use self::models::{
    CatalogResponse, MetaDetail, MetaPreview, MetaResponse, MetaVideo, StreamBehaviorHints,
    StreamItem, StreamResponse, TorrentStreamRef,
};
use crate::state::AppState;
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};

pub(crate) use self::index::LocalIndex;
pub(crate) use self::scanner::scan_background;

pub(crate) fn get_router() -> Router<AppState> {
    Router::new()
        .route("/manifest.json", get(handle_manifest))
        .route("/catalog/{type}/{id}", get(handle_catalog))
        .route("/catalog/{type}/{id}/{extra}", get(handle_catalog_extra))
        .route("/meta/{type}/{id}", get(handle_meta))
        .route("/stream/{type}/{id}", get(handle_stream))
}

async fn handle_manifest() -> Json<manifest::Manifest> {
    Json(get_manifest())
}

async fn handle_catalog(
    state: State<AppState>,
    path: Path<(String, String)>,
) -> Json<CatalogResponse> {
    let (type_, id_ext) = path.0;
    let id = id_ext.strip_suffix(".json").unwrap_or(&id_ext).to_string();
    dispatch_catalog(state, type_, id, None)
}

async fn handle_catalog_extra(
    state: State<AppState>,
    Path((type_, id, extra_ext)): Path<(String, String, String)>,
) -> Json<CatalogResponse> {
    let extra = extra_ext
        .strip_suffix(".json")
        .unwrap_or(&extra_ext)
        .to_string();
    dispatch_catalog(state, type_, id, Some(extra))
}

fn dispatch_catalog(
    State(state): State<AppState>,
    type_: String,
    _id: String,
    _extra: Option<String>,
) -> Json<CatalogResponse> {
    // Simple catalog: Return everything matching the type
    let items = state
        .local_index
        .items
        .read()
        .expect("local index items lock poisoned");
    let metas: Vec<MetaPreview> = items
        .values()
        .flatten()
        .filter(|item| item.metadata.type_ == type_)
        // De-duplicate by IMDB ID
        .fold(std::collections::HashMap::new(), |mut acc, item| {
            let key = item.imdb_id.clone().unwrap_or_else(|| item.path.clone());
            acc.entry(key).or_insert(item);
            acc
        })
        .values()
        .map(|item| MetaPreview {
            id: item.id.clone(),
            r#type: item.metadata.type_.clone(),
            name: item
                .metadata
                .name
                .clone()
                .unwrap_or_else(|| "Unknown".to_string()),
            poster: None,
            description: format!("Local file: {}", item.path),
        })
        .collect();

    Json(CatalogResponse { metas })
}

async fn handle_meta(
    State(state): State<AppState>,
    Path((type_, id_ext)): Path<(String, String)>,
) -> Json<MetaResponse> {
    let id = id_ext.strip_suffix(".json").unwrap_or(&id_ext).to_string();

    // Handle bt: prefix - query engine directly
    if id.starts_with("bt:") {
        let info_hash = id.strip_prefix("bt:").unwrap_or(&id).to_lowercase();
        if let Some(engine) = state.stream_engine().get_engine(&info_hash).await {
            let stats = engine.get_statistics().await;
            let videos: Vec<MetaVideo> = stats
                .files
                .iter()
                .enumerate()
                .map(|(idx, file)| MetaVideo {
                    id: format!("{id}:{idx}"),
                    title: Some(file.name.clone()),
                    released: Some("2020-01-01T00:00:00.000Z".to_string()),
                    season: None,
                    episode: None,
                    stream: Some(TorrentStreamRef {
                        info_hash: info_hash.clone(),
                        file_idx: idx,
                    }),
                })
                .collect();

            let meta = MetaDetail {
                id,
                r#type: type_,
                name: Some(if stats.name.is_empty() {
                    "Torrent".to_string()
                } else {
                    stats.name
                }),
                poster: None,
                description: Some("Torrent stream".to_string()),
                show_as_videos: Some(true),
                videos,
            };
            return Json(MetaResponse { meta: Some(meta) });
        }
        return Json(MetaResponse { meta: None });
    }

    // Handle local: prefix - use local index
    let items_map = state
        .local_index
        .items
        .read()
        .expect("local index items lock poisoned");
    if let Some(items) = items_map.get(&id)
        && let Some(first) = items.first()
    {
        // For series, we might want to list videos
        let videos: Vec<MetaVideo> = items
            .iter()
            .map(|it| {
                let season = it.metadata.season.unwrap_or(1);
                let episode = it
                    .metadata
                    .episode
                    .as_ref()
                    .and_then(|v| v.first().copied())
                    .unwrap_or(1);
                MetaVideo {
                    id: format!("{id}:{season}:{episode}"),
                    title: it.metadata.name.clone(),
                    season: Some(season),
                    episode: Some(episode),
                    released: Some(it.metadata.year.unwrap_or(2020).to_string()),
                    stream: None,
                }
            })
            .collect();

        let meta = MetaDetail {
            id,
            r#type: type_,
            name: first.metadata.name.clone(),
            poster: None,
            description: None,
            show_as_videos: None,
            videos,
        };
        return Json(MetaResponse { meta: Some(meta) });
    }
    Json(MetaResponse { meta: None })
}

async fn handle_stream(
    State(state): State<AppState>,
    Path((_type, id_ext)): Path<(String, String)>,
) -> Json<StreamResponse> {
    let id = id_ext.strip_suffix(".json").unwrap_or(&id_ext);

    let parts: Vec<&str> = id.split(':').collect();
    if parts.len() < 2 {
        return Json(StreamResponse {
            streams: Vec::new(),
        });
    }

    // Handle bt: prefix - query engine directly
    if parts[0] == "bt" {
        let info_hash = parts[1].to_lowercase();
        let file_idx: usize = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);

        if let Some(engine) = state.stream_engine().get_engine(&info_hash).await {
            let stats = engine.get_statistics().await;
            let file_name = stats
                .files
                .get(file_idx)
                .map_or_else(|| "Unknown".to_string(), |f| f.name.clone());
            return Json(StreamResponse {
                streams: vec![StreamItem {
                    title: file_name,
                    info_hash: Some(info_hash),
                    file_idx: Some(file_idx),
                    url: None,
                    behavior_hints: StreamBehaviorHints {
                        not_web_ready: false,
                        binge_group: None,
                    },
                }],
            });
        }
        return Json(StreamResponse {
            streams: Vec::new(),
        });
    }

    // Handle local: prefix - use local index
    let base_id = format!("{}:{}", parts[0], parts[1]); // local:ttXXXX

    let items_map = state
        .local_index
        .items
        .read()
        .expect("local index items lock poisoned");
    if let Some(items) = items_map.get(&base_id) {
        // Filter by S:E if present
        let target_season = parts.get(2).and_then(|s| s.parse::<i32>().ok());
        let target_episode = parts.get(3).and_then(|s| s.parse::<i32>().ok());

        let matched_item = items.iter().find(|it| {
            if target_season.is_some() {
                it.metadata.season == target_season
                    && it
                        .metadata
                        .episode
                        .as_ref()
                        .is_some_and(|e| e.contains(&target_episode.unwrap_or(1)))
            } else {
                true // Movie match
            }
        });

        if let Some(item) = matched_item {
            if let (Some(info_hash), Some(file_idx)) = (&item.info_hash, item.file_idx) {
                // Torrent Stream
                return Json(StreamResponse {
                    streams: vec![StreamItem {
                        title: item
                            .metadata
                            .name
                            .clone()
                            .unwrap_or_else(|| "Torrent File".to_string()),
                        info_hash: Some(info_hash.clone()),
                        file_idx: Some(file_idx),
                        url: None,
                        behavior_hints: StreamBehaviorHints {
                            not_web_ready: false, // Can stream via enginefs
                            binge_group: None,
                        },
                    }],
                });
            }

            // Check for archive path
            if item.path.starts_with("archive:") {
                // path format: archive:{abs_path}|{internal_path}
                let content = item.path.strip_prefix("archive:").unwrap_or(&item.path);
                if let Some((archive_path, internal_path)) = content.split_once('|') {
                    // Use /zip/stream generic endpoint (works for all supported types via factory)
                    let url = format!(
                        "{}/zip/stream?archive={}&file={}",
                        state.base_url,
                        urlencoding::encode(archive_path),
                        urlencoding::encode(internal_path)
                    );

                    return Json(StreamResponse {
                        streams: vec![StreamItem {
                            title: item
                                .metadata
                                .name
                                .clone()
                                .unwrap_or_else(|| "Archive File".to_string()),
                            info_hash: None,
                            file_idx: None,
                            url: Some(url),
                            behavior_hints: StreamBehaviorHints {
                                not_web_ready: false, // It IS web ready via our HTTP proxy
                                binge_group: Some(format!("archive:{archive_path}")),
                            },
                        }],
                    });
                }
            }

            // Local File Stream
            return Json(StreamResponse {
                streams: vec![StreamItem {
                    title: "Local File".to_string(),
                    info_hash: None,
                    file_idx: None,
                    url: Some(format!("file://{}", item.path)),
                    behavior_hints: StreamBehaviorHints {
                        not_web_ready: true,
                        binge_group: None,
                    },
                }],
            });
        }
    }

    Json(StreamResponse {
        streams: Vec::new(),
    })
}

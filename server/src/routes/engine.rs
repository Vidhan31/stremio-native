use crate::error::{AppError, AppResult};
use crate::routes::compat;
use crate::state::AppState;
use axum::extract::{Json, State};
use enginefs::backend::{EngineStats, TorrentHandle};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct EmptyResponse {}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct EngineStatsResponse {
    #[serde(flatten)]
    pub stats: EngineStats,
    #[serde(rename = "guessedFileIdx", skip_serializing_if = "Option::is_none")]
    pub guessed_file_idx: Option<usize>,
}

#[derive(Deserialize)]
pub(crate) struct CreateEngineRequest {
    pub from: Option<String>,
    #[serde(alias = "blob")]
    pub torrent: Option<String>,
    pub announce: Option<Vec<String>>,
    #[serde(rename = "peerSearch")]
    pub peer_search: Option<PeerSearchBody>,
    #[serde(rename = "fileMustInclude", default)]
    pub file_must_include: Vec<String>,
    #[serde(rename = "guessFileIdx")]
    pub guess_file_idx: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub(crate) struct PeerSearchBody {
    #[serde(default)]
    pub sources: Vec<String>,
}

pub(crate) async fn create_engine(
    State(state): State<AppState>,
    Json(payload): Json<CreateEngineRequest>,
) -> AppResult<Json<EngineStatsResponse>> {
    let source = if let Some(hex_str) = payload.torrent {
        let bytes = hex::decode(hex_str)
            .map_err(|e| AppError::bad_request(format!("Invalid hex blob: {e}")))?;
        enginefs::backend::TorrentSource::Bytes(bytes)
    } else if let Some(from) = payload.from {
        enginefs::backend::TorrentSource::Url(from)
    } else {
        return Err(AppError::bad_request("Missing 'from' or 'torrent' field"));
    };

    let trackers = merged_trackers(payload.announce, payload.peer_search);
    let file_must_include = payload.file_must_include;
    let should_guess = guess_file_idx_requested(payload.guess_file_idx.as_ref());

    let engine = state
        .stream_engine()
        .add_torrent(source, Some(trackers))
        .await
        .map_err(|e| AppError::internal(anyhow::anyhow!("Failed to add torrent: {e}")))?;

    let stats = stats_with_guess(&engine, &file_must_include, should_guess).await;
    Ok(Json(stats))
}

pub(crate) async fn list_engines(State(state): State<AppState>) -> Json<Vec<String>> {
    let engines = state.stream_engine().list_engines().await;
    Json(engines)
}

pub(crate) async fn remove_engine(
    State(state): State<AppState>,
    axum::extract::Path(info_hash): axum::extract::Path<String>,
) -> Json<EmptyResponse> {
    state
        .stream_engine()
        .remove_engine(&info_hash.to_lowercase())
        .await;
    Json(EmptyResponse {})
}

pub(crate) async fn remove_all_engines(State(state): State<AppState>) -> Json<EmptyResponse> {
    let engine_fs = state.stream_engine();
    let engines = engine_fs.list_engines().await;
    for ih in engines {
        engine_fs.remove_engine(&ih).await;
    }
    Json(EmptyResponse {})
}

#[derive(Deserialize)]
pub(crate) struct CreateMagnetRequest {
    pub stream: Option<CreateMagnetStream>,
    #[serde(rename = "peerSearch")]
    pub peer_search: Option<PeerSearchBody>,
    #[serde(rename = "fileMustInclude", default)]
    pub file_must_include: Vec<String>,
    #[serde(rename = "guessFileIdx")]
    pub guess_file_idx: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub(crate) struct CreateMagnetStream {
    #[serde(rename = "infoHash")]
    pub info_hash: Option<String>,
}

pub(crate) async fn create_magnet(
    State(state): State<AppState>,
    axum::extract::Path(info_hash): axum::extract::Path<String>,
    Json(payload): Json<CreateMagnetRequest>,
) -> AppResult<Json<EngineStatsResponse>> {
    let ih = payload
        .stream
        .as_ref()
        .and_then(|s| s.info_hash.as_ref())
        .map_or(info_hash.as_str(), |s| s.as_str());

    let magnet = format!("magnet:?xt=urn:btih:{ih}");
    let source = enginefs::backend::TorrentSource::Url(magnet);

    let trackers = merged_trackers(None, payload.peer_search);
    let file_must_include = payload.file_must_include;
    let should_guess = guess_file_idx_requested(payload.guess_file_idx.as_ref());

    let engine = state
        .stream_engine()
        .add_torrent(source, Some(trackers))
        .await
        .map_err(|e| AppError::internal(anyhow::anyhow!("Failed to add magnet: {e}")))?;

    let stats = stats_with_guess(&engine, &file_must_include, should_guess).await;
    Ok(Json(stats))
}

pub(crate) async fn create_magnet_get(
    State(state): State<AppState>,
    axum::extract::Path(info_hash): axum::extract::Path<String>,
) -> AppResult<Json<EngineStatsResponse>> {
    let magnet = format!("magnet:?xt=urn:btih:{info_hash}");
    let source = enginefs::backend::TorrentSource::Url(magnet);

    let engine = state
        .stream_engine()
        .add_torrent(source, None)
        .await
        .map_err(|e| AppError::internal(anyhow::anyhow!("Failed to add magnet: {e}")))?;

    let stats = stats_with_guess(&engine, &[], false).await;
    Ok(Json(stats))
}

fn merged_trackers(
    announce: Option<Vec<String>>,
    peer_search: Option<PeerSearchBody>,
) -> Vec<String> {
    let mut sources = announce.unwrap_or_default();
    if let Some(peer_search) = peer_search {
        sources.extend(peer_search.sources);
    }
    compat::normalize_tracker_sources(sources)
}

fn guess_file_idx_requested(value: Option<&serde_json::Value>) -> bool {
    match value {
        Some(serde_json::Value::Bool(false)) | None => false,
        Some(serde_json::Value::Null) => false,
        Some(_) => true,
    }
}

async fn stats_with_guess<H>(
    engine: &Arc<enginefs::engine::Engine<H>>,
    filters: &[String],
    should_guess: bool,
) -> EngineStatsResponse
where
    H: TorrentHandle,
{
    let stats = engine.get_statistics().await;

    if filters.is_empty() && !should_guess {
        return EngineStatsResponse {
            stats,
            guessed_file_idx: None,
        };
    }

    let files = engine.handle.get_files().await;
    let candidates = files
        .iter()
        .enumerate()
        .map(|(index, file)| compat::FileCandidate {
            index,
            name: file.name.clone(),
            length: file.length,
        })
        .collect::<Vec<_>>();

    let guessed_file_idx = compat::resolve_file_idx("-1", &candidates, filters).ok();

    EngineStatsResponse {
        stats,
        guessed_file_idx,
    }
}

use crate::state::AppState;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use enginefs::backend::{SubtitleTrack, TorrentHandle};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use tokio::io::AsyncReadExt;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RpcResponse<T> {
    pub error: Option<String>,
    pub result: Option<T>,
}

impl<T> RpcResponse<T> {
    pub(crate) fn success(result: T) -> Self {
        Self {
            error: None,
            result: Some(result),
        }
    }

    pub(crate) fn error(msg: impl Into<String>) -> Self {
        Self {
            error: Some(msg.into()),
            result: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OpensubHashResult {
    pub hash: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SubtitleTrackEntry {
    pub id: usize,
    pub lang: &'static str,
    pub label: String,
    pub url: String,
}

#[derive(Deserialize)]
pub(crate) struct OpensubHashQuery {
    #[serde(rename = "videoUrl")]
    pub video_url: Option<String>,
}

fn parse_video_url(url: &str) -> Option<(String, usize)> {
    let mut parts = url.split('/');
    while let Some(part) = parts.next() {
        if part.len() == 40 && hex::decode(part).is_ok() {
            let info_hash = part.to_lowercase();
            if let Some(next_part) = parts.next() {
                let file_part = next_part.split('?').next().unwrap_or("");
                if let Ok(idx) = file_part.parse::<usize>() {
                    return Some((info_hash, idx));
                }
            }
            break;
        }
    }
    None
}

pub(crate) async fn opensub_hash(
    State(state): State<AppState>,
    Query(query): Query<OpensubHashQuery>,
) -> Json<RpcResponse<OpensubHashResult>> {
    let url = query.video_url.unwrap_or_default();

    if let Some((info_hash, file_idx)) = parse_video_url(&url)
        && let Some(engine) = state.stream_engine().get_engine(&info_hash).await
    {
        return match engine.get_opensub_hash(file_idx).await {
            Ok(hash) => {
                let size = engine
                    .handle
                    .get_files()
                    .await
                    .get(file_idx)
                    .map_or(0, |file| file.length);
                Json(RpcResponse::success(OpensubHashResult { hash, size }))
            }
            Err(e) => Json(RpcResponse::error(e.to_string())),
        };
    }

    Json(RpcResponse::error("Could not identify file from URL"))
}

pub(crate) async fn opensub_hash_path(
    State(state): State<AppState>,
    Path((info_hash, file_idx)): Path<(String, usize)>,
) -> Json<RpcResponse<OpensubHashResult>> {
    let info_hash = info_hash.to_lowercase();
    if let Some(engine) = state.stream_engine().get_engine(&info_hash).await {
        return match engine.get_opensub_hash(file_idx).await {
            Ok(hash) => {
                let size = engine
                    .handle
                    .get_files()
                    .await
                    .get(file_idx)
                    .map_or(0, |file| file.length);
                Json(RpcResponse::success(OpensubHashResult { hash, size }))
            }
            Err(e) => Json(RpcResponse::error(e.to_string())),
        };
    }
    Json(RpcResponse::error("Engine not found"))
}

#[derive(Deserialize)]
pub(crate) struct SubtitlesTracksQuery {
    #[serde(rename = "subsUrl")]
    pub subs_url: Option<String>,
}

pub(crate) async fn subtitles_tracks(
    State(state): State<AppState>,
    Query(query): Query<SubtitlesTracksQuery>,
) -> Json<RpcResponse<Vec<SubtitleTrackEntry>>> {
    let url = query.subs_url.unwrap_or_default();
    let info_hash = url
        .split('/')
        .find(|part| part.len() == 40 && hex::decode(part).is_ok())
        .map(str::to_lowercase);

    if let Some(info_hash) = info_hash
        && let Some(engine) = state.stream_engine().get_engine(&info_hash).await
    {
        let tracks: Vec<SubtitleTrack> = engine.find_subtitle_tracks().await;

        let result = tracks
            .into_iter()
            .map(|t| SubtitleTrackEntry {
                url: format!("/{info_hash}/{}/subtitles.vtt", t.id),
                id: t.id,
                lang: "Unknown",
                label: t.name,
            })
            .collect();

        return Json(RpcResponse::success(result));
    }

    Json(RpcResponse::success(Vec::new()))
}

pub(crate) async fn get_subtitles_vtt(
    State(state): State<AppState>,
    Path((info_hash, file_idx)): Path<(String, usize)>,
) -> Response {
    if let Some(engine) = state.stream_engine().get_engine(&info_hash).await {
        if file_idx >= 1000 {
            let files = engine.handle.get_files().await;
            let mut largest_idx = None;
            let mut max_len = 0;
            for (idx, f) in files.iter().enumerate() {
                if f.length > max_len {
                    max_len = f.length;
                    largest_idx = Some(idx);
                }
            }

            if let Some(main_video_idx) = largest_idx {
                match engine
                    .extract_embedded_subtitle(main_video_idx, file_idx)
                    .await
                {
                    Ok(content) => {
                        return (
                            [
                                (header::CONTENT_TYPE, "text/vtt"),
                                (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
                            ],
                            content,
                        )
                            .into_response();
                    }
                    Err(e) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("Extraction failed: {e}"),
                        )
                            .into_response();
                    }
                }
            }
        }

        if let Some(mut file) = engine.get_file(file_idx, 0, 0).await {
            let mut content = String::new();
            if file.read_to_string(&mut content).await.is_ok() {
                let vtt_content = enginefs::subtitles::convert_to_vtt(&content);

                return (
                    [
                        (header::CONTENT_TYPE, "text/vtt"),
                        (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
                    ],
                    vtt_content,
                )
                    .into_response();
            }
        }
    }
    StatusCode::NOT_FOUND.into_response()
}

static RE_SUBTITLE_TIMECODE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(\d{2}):(\d{2}):(\d{2})([,.])(\d{3})").expect("valid regex")
});

#[derive(Deserialize)]
pub(crate) struct ProxySubtitlesQuery {
    pub from: Option<String>,
    pub offset: Option<i64>,
}

pub(crate) async fn proxy_subtitles_vtt(
    State(state): State<AppState>,
    Query(query): Query<ProxySubtitlesQuery>,
) -> Response {
    proxy_subtitles_response(&state.http_client, "vtt", query).await
}

pub(crate) async fn proxy_subtitles_ext(
    State(state): State<AppState>,
    Path(ext): Path<String>,
    Query(query): Query<ProxySubtitlesQuery>,
) -> Response {
    proxy_subtitles_response(&state.http_client, &ext, query).await
}

async fn proxy_subtitles_response(
    client: &reqwest::Client,
    ext: &str,
    query: ProxySubtitlesQuery,
) -> Response {
    let from_url = match query.from {
        Some(url) => url,
        None => {
            return (StatusCode::BAD_REQUEST, "Missing 'from' parameter").into_response();
        }
    };

    let resp = match client.get(&from_url).send().await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("Failed to fetch subtitles: {e}"),
            )
                .into_response();
        }
    };

    use futures_util::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut bytes = Vec::new();
    const MAX_SUB_SIZE: usize = 5 * 1024 * 1024;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    format!("Failed to read subtitles stream: {e}"),
                )
                    .into_response();
            }
        };
        if bytes.len() + chunk.len() > MAX_SUB_SIZE {
            return (
                StatusCode::BAD_REQUEST,
                "Subtitle file exceeds maximum allowed size (5MB)",
            )
                .into_response();
        }
        bytes.extend_from_slice(&chunk);
    }
    let content = String::from_utf8_lossy(&bytes);

    let offset = query.offset.unwrap_or(0);
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    match ext.as_str() {
        "vtt" => {
            let vtt_content = enginefs::subtitles::convert_to_vtt(&content);
            let shifted = apply_subtitle_offset(&vtt_content, offset);
            Response::builder()
                .header(header::CONTENT_TYPE, "text/vtt")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(axum::body::Body::from(shifted))
                .unwrap_or_else(|_| {
                    (StatusCode::INTERNAL_SERVER_ERROR, "Response build error").into_response()
                })
        }
        "srt" => {
            let shifted = apply_subtitle_offset(&content, offset);
            Response::builder()
                .header(header::CONTENT_TYPE, "application/x-subrip")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(axum::body::Body::from(shifted))
                .unwrap_or_else(|_| {
                    (StatusCode::INTERNAL_SERVER_ERROR, "Response build error").into_response()
                })
        }
        _ => (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("Unsupported subtitle extension: {ext}"),
        )
            .into_response(),
    }
}

fn apply_subtitle_offset(content: &str, offset_ms: i64) -> String {
    if offset_ms == 0 {
        return content.to_string();
    }

    RE_SUBTITLE_TIMECODE
        .replace_all(content, |caps: &regex::Captures| {
            let hours = caps[1].parse::<i64>().unwrap_or(0);
            let minutes = caps[2].parse::<i64>().unwrap_or(0);
            let seconds = caps[3].parse::<i64>().unwrap_or(0);
            let millis = caps[5].parse::<i64>().unwrap_or(0);
            let total =
                (hours * 3_600_000 + minutes * 60_000 + seconds * 1_000 + millis + offset_ms)
                    .max(0);
            let h = total / 3_600_000;
            let m = (total % 3_600_000) / 60_000;
            let s = (total % 60_000) / 1_000;
            let ms = total % 1_000;
            format!("{h:02}:{m:02}:{s:02}{}{ms:03}", &caps[4])
        })
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_video_url_valid() {
        let url = "http://localhost:11470/0123456789abcdef0123456789abcdef01234567/3?tr=udp";
        let parsed = parse_video_url(url);
        assert_eq!(
            parsed,
            Some(("0123456789abcdef0123456789abcdef01234567".to_string(), 3))
        );
    }

    #[test]
    fn test_parse_video_url_invalid() {
        assert_eq!(parse_video_url("http://localhost:11470/not_a_hash/0"), None);
    }
}

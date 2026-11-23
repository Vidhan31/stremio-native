use crate::error::{AppError, AppResult};
use crate::state::AppState;
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query},
    http::header,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use tokio::process::Command;
use tokio_util::io::ReaderStream;

#[derive(Debug, Deserialize)]
pub(crate) struct TranscodeParams {
    pub video: String,
    pub time: Option<f64>,
    #[serde(rename = "audioTrack")]
    pub _audio_track: Option<usize>,
    pub fmp4: Option<String>,
    pub _subtitles: Option<String>,
    #[serde(rename = "subtitlesDelay")]
    pub _subtitles_delay: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct PlayerParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<String>,
    #[serde(rename = "audioTrack", alias = "audio_track", skip_serializing_if = "Option::is_none")]
    pub audio_track: Option<usize>,
}

struct ProcessStream<S> {
    inner: S,
    _child: tokio::process::Child,
}

impl<S: futures_util::Stream + Unpin> futures_util::Stream for ProcessStream<S> {
    type Item = S::Item;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PlayerControlResponse {
    #[serde(rename = "deviceId")]
    pub device_id: String,
    pub status: &'static str,
    pub params: PlayerParams,
}

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_devices))
        .route("/transcode", get(transcode))
        .route("/convert", get(transcode))
        .route("/{devID}", get(get_device))
        .route("/{devID}/player", get(player_control).post(player_control))
}

pub(crate) async fn list_devices(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> impl IntoResponse {
    let devices = state.devices.read().await;
    Json(devices.clone())
}

pub(crate) async fn get_device(Path(dev_id): Path<String>) -> AppResult<Json<crate::ssdp::Device>> {
    Err(AppError::not_found(format!("Device {dev_id} not found")))
}

pub(crate) async fn transcode(Query(params): Query<TranscodeParams>) -> AppResult {
    let video_url = params.video;
    let offset = params.time.unwrap_or(0.0);
    let is_fmp4 = params.fmp4.is_some();
    let offset_str = offset.to_string();

    let mut cmd = Command::new("ffmpeg");
    cmd.kill_on_drop(true);
    cmd.args(["-copyts", "-ss", &offset_str, "-i", &video_url])
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-ac",
            "2",
            "-threads",
            "0",
        ]);

    if is_fmp4 {
        cmd.args(["-movflags", "frag_keyframe+empty_moov", "-f", "mp4"]);
    } else {
        cmd.args(["-f", "matroska"]);
    }

    cmd.arg("pipe:1");
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());

    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::internal(anyhow::anyhow!("Failed to spawn ffmpeg: {e}")))?;

    let stdout = child.stdout.take().expect("Failed to open stdout");
    let stream = ProcessStream {
        inner: ReaderStream::new(stdout),
        _child: child,
    };

    let content_type = if is_fmp4 {
        "video/mp4"
    } else {
        "video/x-matroska"
    };

    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::TRANSFER_ENCODING, "chunked")
        .header("transferMode.dlna.org", "Streaming")
        .header(
            "contentFeatures.dlna.org",
            "DLNA.ORG_OP=01;DLNA.ORG_CI=1;DLNA.ORG_FLAGS=01300000000000000000000000000000",
        )
        .body(Body::from_stream(stream))
        .map_err(|e| AppError::internal(anyhow::anyhow!("Failed to build response: {e}")))
}

pub(crate) async fn player_control(
    method: axum::http::Method,
    Path(dev_id): Path<String>,
    Query(query_params): Query<PlayerParams>,
    body: Option<Json<PlayerParams>>,
) -> Json<PlayerControlResponse> {
    let params = if method == axum::http::Method::POST {
        body.map_or(query_params, |Json(b)| b)
    } else {
        query_params
    };

    Json(PlayerControlResponse {
        device_id: dev_id,
        status: "not_implemented",
        params,
    })
}

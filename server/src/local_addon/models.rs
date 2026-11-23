use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CatalogResponse {
    pub metas: Vec<MetaPreview>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MetaPreview {
    pub id: String,
    pub r#type: String,
    pub name: String,
    pub poster: Option<String>,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MetaResponse {
    pub meta: Option<MetaDetail>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MetaDetail {
    pub id: String,
    pub r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub poster: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "showAsVideos", skip_serializing_if = "Option::is_none")]
    pub show_as_videos: Option<bool>,
    pub videos: Vec<MetaVideo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MetaVideo {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub released: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub season: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<TorrentStreamRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TorrentStreamRef {
    #[serde(rename = "infoHash")]
    pub info_hash: String,
    #[serde(rename = "fileIdx")]
    pub file_idx: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StreamResponse {
    pub streams: Vec<StreamItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StreamItem {
    pub title: String,
    #[serde(rename = "infoHash", skip_serializing_if = "Option::is_none")]
    pub info_hash: Option<String>,
    #[serde(rename = "fileIdx", skip_serializing_if = "Option::is_none")]
    pub file_idx: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(rename = "behaviorHints")]
    pub behavior_hints: StreamBehaviorHints,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StreamBehaviorHints {
    #[serde(rename = "notWebReady")]
    pub not_web_ready: bool,
    #[serde(rename = "bingeGroup", skip_serializing_if = "Option::is_none")]
    pub binge_group: Option<String>,
}

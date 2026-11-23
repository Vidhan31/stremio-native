use std::sync::Arc;
use thiserror::Error;

/// Core error type for EngineFS operations.
#[derive(Debug, Clone, Error)]
pub enum EngineError {
    #[error("Torrent not found: {0}")]
    TorrentNotFound(String),

    #[error("File not found: {0}")]
    FileNotFound(String),

    #[error("File index {index} out of range (total files: {max})")]
    FileIndexOutOfRange { index: usize, max: usize },

    #[error("Session error: {0}")]
    Session(String),

    #[error("Alert channel error: {0}")]
    Alert(String),

    #[error("Playback request was superseded: {0}")]
    Superseded(String),

    #[error("Operation timed out: {0}")]
    Timeout(String),

    #[error("Extraction failed: {0}")]
    Extraction(String),

    #[error("Media probe failed: {0}")]
    Probe(String),

    #[error("Invalid embedded track: {0}")]
    InvalidTrack(String),

    #[error("I/O error: {0}")]
    Io(Arc<std::io::Error>),

    #[error("Network error: {0}")]
    Network(Arc<reqwest::Error>),

    #[error("JSON error: {0}")]
    Json(Arc<serde_json::Error>),

    #[error("Internal engine error: {0}")]
    Other(String),
}

impl From<std::io::Error> for EngineError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(Arc::new(err))
    }
}

impl From<reqwest::Error> for EngineError {
    fn from(err: reqwest::Error) -> Self {
        Self::Network(Arc::new(err))
    }
}

impl From<serde_json::Error> for EngineError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(Arc::new(err))
    }
}

impl From<anyhow::Error> for EngineError {
    fn from(err: anyhow::Error) -> Self {
        Self::Other(err.to_string())
    }
}

pub type Result<T, E = EngineError> = std::result::Result<T, E>;

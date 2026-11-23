use axum::{Json, Router, extract::Query, routing::get};
use std::collections::HashMap;

use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/getAll", get(get_all))
        .route("/get", get(get_by_id))
        .route("/add", get(add))
        .route("/pause", get(pause))
        .route("/resume", get(resume))
        .route("/remove", get(remove))
}

async fn get_all() -> Json<Vec<String>> {
    Json(Vec::new())
}

async fn get_by_id(Query(params): Query<HashMap<String, String>>) -> AppResult {
    match params.get("id") {
        Some(_) => Err(AppError::not_found("there is no download with this id")),
        None => Err(AppError::bad_request("missing id")),
    }
}

async fn add(Query(params): Query<HashMap<String, String>>) -> AppResult {
    match params.get("url") {
        Some(_) => Err(AppError::unsupported("HTTP file downloader")),
        None => Err(AppError::bad_request("missing url")),
    }
}

async fn pause() -> AppResult {
    Err(AppError::unsupported("HTTP file downloader"))
}

async fn resume() -> AppResult {
    Err(AppError::unsupported("HTTP file downloader"))
}

async fn remove() -> AppResult {
    Err(AppError::unsupported("HTTP file downloader"))
}

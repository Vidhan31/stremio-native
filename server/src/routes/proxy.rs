use crate::state::AppState;
use axum::{
    Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderName, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use reqwest::Method;
use std::collections::HashMap;
use std::fmt::Write as _;
use url::Url;

static ALLOWED_REQUEST_HEADERS: &[HeaderName] = &[
    header::ACCEPT,
    header::ACCEPT_ENCODING,
    header::ACCEPT_LANGUAGE,
    header::RANGE,
    header::IF_RANGE,
    header::USER_AGENT,
];

static ALLOWED_RESPONSE_HEADERS: &[HeaderName] = &[
    header::ACCEPT_RANGES,
    header::CONTENT_TYPE,
    header::CONTENT_LENGTH,
    header::CONTENT_RANGE,
    header::LAST_MODIFIED,
    header::ETAG,
    header::SERVER,
    header::DATE,
];

#[derive(Debug, Clone)]
pub(crate) struct ProxyTarget {
    pub url: Url,
    pub custom_headers: HashMap<String, String>,
    pub custom_response_headers: HashMap<String, String>,
}

impl ProxyTarget {
    pub(crate) fn parse(
        rest: &str,
        raw_query: Option<&str>,
        params: &HashMap<String, String>,
    ) -> Result<Self, &'static str> {
        let mut target_url_str = String::new();
        let mut custom_headers = HashMap::new();
        let mut custom_response_headers = HashMap::new();
        let mut is_path_format = false;

        if let Some(d) = params.get("d") {
            target_url_str.clone_from(d);
        } else {
            is_path_format = true;
            let (query_seg, path_seg) = match rest.split_once('/') {
                Some((q, p)) => (q, p),
                None => (rest, ""),
            };

            for (key, val) in url::form_urlencoded::parse(query_seg.as_bytes()) {
                match key.as_ref() {
                    "d" => target_url_str = val.into_owned(),
                    "h" => {
                        if let Some((name, value)) = val.split_once(':') {
                            custom_headers
                                .insert(name.trim().to_string(), value.trim().to_string());
                        }
                    }
                    "r" => {
                        if let Some((name, value)) = val.split_once(':') {
                            custom_response_headers
                                .insert(name.trim().to_string(), value.trim().to_string());
                        }
                    }
                    _ => {}
                }
            }

            if !target_url_str.is_empty() {
                if !path_seg.is_empty() {
                    if !target_url_str.ends_with('/') {
                        target_url_str.push('/');
                    }
                    target_url_str.push_str(path_seg);
                }
            } else {
                target_url_str = rest.to_string();
            }
        }

        let mut url = Url::parse(&target_url_str).map_err(|_| "Invalid target URL")?;

        if is_path_format && let Some(q) = raw_query {
            url.set_query(Some(q));
        }

        Ok(Self {
            url,
            custom_headers,
            custom_response_headers,
        })
    }
}

pub(crate) fn router() -> Router<AppState> {
    Router::new().route("/{*rest}", any(proxy_handler))
}

pub(crate) async fn proxy_handler(
    State(state): State<AppState>,
    Path(rest): Path<String>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    method: Method,
) -> impl IntoResponse {
    let target = match ProxyTarget::parse(&rest, raw_query.as_deref(), &params) {
        Ok(t) => t,
        Err(err) => return (StatusCode::BAD_REQUEST, err).into_response(),
    };

    let mut req_builder = state.proxy_http_client.request(method, target.url.clone());

    for header_name in ALLOWED_REQUEST_HEADERS {
        if let Some(value) = headers.get(header_name) {
            req_builder = req_builder.header(header_name.clone(), value);
        }
    }

    for (name, value) in target.custom_headers {
        req_builder = req_builder.header(name, value);
    }

    let response = match req_builder.send().await {
        Ok(resp) => resp,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("Proxy error: {e}")).into_response(),
    };

    let status = response.status();
    let mut res_builder = Response::builder().status(status);

    let res_headers = response.headers().clone();
    let content_type = res_headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let is_playlist = target.url.path().ends_with(".m3u8")
        || target.url.path().ends_with(".m3u")
        || content_type.contains("mpegurl");

    for header_name in ALLOWED_RESPONSE_HEADERS {
        // Do not forward upstream Content-Length for playlists, as the rewritten playlist body size differs
        if is_playlist && header_name == header::CONTENT_LENGTH {
            continue;
        }
        if let Some(value) = res_headers.get(header_name) {
            res_builder = res_builder.header(header_name.clone(), value);
        }
    }

    for (name, value) in target.custom_response_headers {
        res_builder = res_builder.header(name, value);
    }

    res_builder = res_builder
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST, OPTIONS")
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "*");

    if is_playlist {
        let body = response.text().await.unwrap_or_default();
        let rewritten = rewrite_playlist(&body, &target.url);
        res_builder = res_builder.header(header::CONTENT_LENGTH, rewritten.len());
        return match res_builder.body(Body::from(rewritten)) {
            Ok(res) => res.into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to build proxy response: {e}"),
            )
                .into_response(),
        };
    }

    let stream = response.bytes_stream();
    match res_builder.body(Body::from_stream(stream)) {
        Ok(res) => res.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to build proxy response: {e}"),
        )
            .into_response(),
    }
}

fn rewrite_playlist(body: &str, base_url: &Url) -> String {
    let mut rewritten = String::with_capacity(body.len() + 128);
    for line in body.lines() {
        if line.is_empty() {
            rewritten.push('\n');
            continue;
        }
        if let Some(stripped) = line.strip_prefix('#') {
            if let Some(start) = line.find("URI=\"") {
                let rest = &line[start + 5..];
                if let Some(end) = rest.find('"') {
                    let uri = &rest[..end];
                    let absolute_uri = if uri.contains("://") {
                        uri.to_string()
                    } else {
                        base_url
                            .join(uri)
                            .map_or_else(|_| uri.to_string(), |u| u.to_string())
                    };
                    let proxy_uri = format!("/proxy/?d={}", urlencoding::encode(&absolute_uri));
                    let _ = writeln!(
                        rewritten,
                        "{}{}{}",
                        &line[..start + 5],
                        proxy_uri,
                        &rest[end..]
                    );
                    continue;
                }
            }
            let _ = writeln!(rewritten, "#{stripped}");
        } else {
            let absolute_uri = if line.contains("://") {
                line.to_string()
            } else {
                base_url
                    .join(line)
                    .map_or_else(|_| line.to_string(), |u| u.to_string())
            };
            let proxy_uri = format!("/proxy/?d={}", urlencoding::encode(&absolute_uri));
            let _ = writeln!(rewritten, "{proxy_uri}");
        }
    }
    rewritten
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_target_parse_standard_query() {
        let mut params = HashMap::new();
        params.insert("d".to_string(), "https://example.com/video.mp4".to_string());
        let target = ProxyTarget::parse("", None, &params).expect("valid target");
        assert_eq!(target.url.as_str(), "https://example.com/video.mp4");
        assert!(target.custom_headers.is_empty());
    }

    #[test]
    fn test_proxy_target_parse_path_format() {
        let params = HashMap::new();
        let target = ProxyTarget::parse(
            "d=https%3A%2F%2Fexample.com&h=Authorization:Bearer%20token/stream.m3u8",
            Some("token=123"),
            &params,
        )
        .expect("valid path target");

        assert_eq!(target.url.path(), "/stream.m3u8");
        assert_eq!(target.url.query(), Some("token=123"));
        assert_eq!(
            target.custom_headers.get("Authorization"),
            Some(&"Bearer token".to_string())
        );
    }

    #[test]
    fn test_rewrite_playlist() {
        let base = Url::parse("https://cdn.example.com/media/playlist.m3u8").unwrap();
        let m3u8 = "#EXTM3U\n#EXT-X-VERSION:3\nsegment1.ts\nsegment2.ts\n";
        let rewritten = rewrite_playlist(m3u8, &base);
        assert!(
            rewritten.contains("/proxy/?d=https%3A%2F%2Fcdn.example.com%2Fmedia%2Fsegment1.ts")
        );
        assert!(
            rewritten.contains("/proxy/?d=https%3A%2F%2Fcdn.example.com%2Fmedia%2Fsegment2.ts")
        );
    }
}

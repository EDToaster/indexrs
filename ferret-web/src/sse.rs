use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::Stream;
use serde::Deserialize;
use tokio::io::{AsyncWriteExt, BufReader};

use crate::AppState;

#[derive(Deserialize)]
pub struct SearchStreamParams {
    pub q: Option<String>,
    #[serde(default = "default_per_page")]
    pub per_page: usize,
    #[serde(default = "default_context")]
    pub context: usize,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

fn default_per_page() -> usize {
    25
}

fn default_context() -> usize {
    2
}

pub async fn search_stream(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(params): Query<SearchStreamParams>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, String)> {
    // Validate repo exists.
    let repo_path = state
        .repo_path(&name)
        .await
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("repo '{name}' not found")))?;

    let query = params
        .q
        .filter(|q| !q.is_empty())
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing or empty query 'q'".into()))?;

    // Connect to daemon.
    let stream = ferret_indexer_daemon::ensure_daemon(state.daemon_bin(), &repo_path, false)
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("daemon unavailable: {e}"),
            )
        })?;

    let request = ferret_indexer_daemon::DaemonRequest::JsonSearch {
        query,
        page: 1,
        per_page: params.per_page,
        context_lines: params.context,
        language: params.language,
        path_glob: params.path,
    };

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Send request.
    let json = serde_json::to_string(&request).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to serialize request: {e}"),
        )
    })?;
    writer
        .write_all(format!("{json}\n").as_bytes())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to send request: {e}"),
            )
        })?;

    let event_stream = async_stream::stream! {
        loop {
            match ferret_indexer_daemon::wire::read_response(&mut reader).await {
                Ok(ferret_indexer_daemon::DaemonResponse::Json { payload }) => {
                    // Parse to determine event type.
                    match serde_json::from_str::<ferret_indexer_daemon::JsonSearchFrame>(&payload) {
                        Ok(ferret_indexer_daemon::JsonSearchFrame::Result { .. }) => {
                            yield Ok(Event::default().event("result").data(payload));
                        }
                        Ok(ferret_indexer_daemon::JsonSearchFrame::Stats { .. }) => {
                            yield Ok(Event::default().event("stats").data(payload));
                        }
                        Err(_) => {
                            // Unknown JSON frame, forward as-is.
                            yield Ok(Event::default().event("result").data(payload));
                        }
                    }
                }
                Ok(ferret_indexer_daemon::DaemonResponse::Done { .. }) => {
                    yield Ok(Event::default().event("done").data("{}"));
                    break;
                }
                Ok(ferret_indexer_daemon::DaemonResponse::Error { message }) => {
                    yield Ok(Event::default().event("error").data(message));
                    break;
                }
                Ok(_) => {
                    // Skip non-JSON frames (Line, Progress, Pong).
                }
                Err(e) => {
                    let msg = format!("daemon read error: {e}");
                    tracing::error!("{msg}");
                    yield Ok(Event::default().event("error").data(msg));
                    break;
                }
            }
        }
    };

    Ok(Sse::new(event_stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

pub async fn status_stream(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, impl IntoResponse> {
    // Validate repo exists.
    let repo_path = state
        .repo_path(&name)
        .await
        .ok_or((StatusCode::NOT_FOUND, format!("repo '{name}' not found")))?;

    let daemon_bin = state.daemon_bin().clone();

    let event_stream = async_stream::stream! {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;

            let status_json = match ferret_indexer_daemon::ensure_daemon(&daemon_bin, &repo_path, false).await {
                Ok(stream) => {
                    let request = ferret_indexer_daemon::DaemonRequest::Status;
                    match ferret_indexer_daemon::send_json_request(stream, &request).await {
                        Ok(result) => {
                            // The first JSON payload is the StatusResponse.
                            result.payloads.into_iter().next().unwrap_or_else(|| {
                                r#"{"status":"unknown","files_indexed":0,"segments":0,"index_bytes":0,"last_indexed_ts":0,"languages":[],"tombstone_ratio":0.0,"path_valid":true}"#.to_string()
                            })
                        }
                        Err(e) => {
                            tracing::warn!("status request failed: {e}");
                            r#"{"status":"offline","files_indexed":0,"segments":0,"index_bytes":0,"last_indexed_ts":0,"languages":[],"tombstone_ratio":0.0,"path_valid":true}"#.to_string()
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!("daemon connect failed for status: {e}");
                    r#"{"status":"offline","files_indexed":0,"segments":0,"index_bytes":0,"last_indexed_ts":0,"languages":[],"tombstone_ratio":0.0,"path_valid":true}"#.to_string()
                }
            };

            yield Ok::<_, Infallible>(Event::default().event("status").data(status_json));
        }
    };

    Ok::<_, (StatusCode, String)>(
        Sse::new(event_stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))),
    )
}

use crate::controlplane;
use crate::tls;
use anyhow::Result;
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, Response, StatusCode},
    routing::any,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
struct FzyHttpState {
    home: PathBuf,
}

pub async fn serve_fzy_control_plane(
    home: PathBuf,
    bind: SocketAddr,
    tls_paths: Option<(PathBuf, PathBuf, Option<PathBuf>)>,
) -> Result<()> {
    let state = Arc::new(FzyHttpState { home });
    let app = Router::new()
        .route("/v1/{*path}", any(handle_fzy_http))
        .with_state(state);

    if let Some((cert_path, key_path, ca_path)) = tls_paths {
        let tls_config = tls::load_server_config(&cert_path, &key_path, ca_path.as_deref())?;
        let rustls_config =
            axum_server::tls_rustls::RustlsConfig::from_config(std::sync::Arc::new(tls_config));
        axum_server::bind_rustls(bind, rustls_config)
            .serve(app.into_make_service())
            .await?;
    } else {
        let listener = tokio::net::TcpListener::bind(bind).await?;
        axum::serve(listener, app).await?;
    }
    Ok(())
}

async fn handle_fzy_http(
    State(state): State<Arc<FzyHttpState>>,
    request: Request,
) -> Result<Response<Body>, (StatusCode, String)> {
    let method = request.method().as_str().to_string();
    let path = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_string())
        .unwrap_or_else(|| request.uri().path().to_string());
    let headers = request.headers().clone();
    let body = request
        .into_body()
        .collect()
        .await
        .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?
        .to_bytes();
    let body = if body.is_empty() {
        None
    } else {
        Some(std::str::from_utf8(&body).map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?)
    };

    let response = controlplane::dispatch_http_raw(
        &state.home,
        &method,
        &path,
        body,
        header_value(&headers, "content-type"),
        header_value(&headers, "accept"),
        header_value(&headers, "authorization"),
    )
    .map_err(internal_error)?;

    response_from_fzy(response)
}

fn response_from_fzy(value: Value) -> Result<Response<Body>, (StatusCode, String)> {
    let status = value
        .get("http_status")
        .and_then(Value::as_u64)
        .and_then(|raw| u16::try_from(raw).ok())
        .and_then(|raw| StatusCode::from_u16(raw).ok())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let content_type = value
        .get("content_type")
        .and_then(Value::as_str)
        .unwrap_or("application/json; charset=utf-8");
    let body = match value.get("body") {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => json!({
            "status": "error",
            "message": "fzy response missing body",
            "control_plane": "rust-http-host"
        })
        .to_string(),
    };
    Response::builder()
        .status(status)
        .header("content-type", content_type)
        .body(Body::from(body))
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
}

fn header_value<'a>(headers: &'a HeaderMap, name: &'static str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn internal_error(err: anyhow::Error) -> (StatusCode, String) {
    let mut message = err.to_string();
    for cause in err.chain().skip(1) {
        message.push_str(": ");
        message.push_str(&cause.to_string());
    }
    (StatusCode::INTERNAL_SERVER_ERROR, message)
}

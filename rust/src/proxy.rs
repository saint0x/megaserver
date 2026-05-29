use crate::state::{self, StatePaths};
use crate::tls;
use anyhow::{Context, Result};
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, Response, StatusCode},
    routing::any,
};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use hyper::upgrade::Upgraded;
use reqwest::Client;
use sha1::{Digest, Sha1};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, protocol::Role};
use tokio_tungstenite::{WebSocketStream, connect_async};

#[derive(Clone)]
struct ProxyState {
    client: Client,
    paths: StatePaths,
    ingress_scheme: String,
}

fn service_target_host(sandbox: Option<&crate::model::SandboxRecord>) -> &str {
    match sandbox {
        Some(sandbox) if sandbox.runtime_kind == "linux-namespace" => {
            sandbox.ip_address.as_deref().unwrap_or("127.0.0.1")
        }
        _ => "127.0.0.1",
    }
}

pub async fn serve(
    paths: StatePaths,
    bind: SocketAddr,
    tls_paths: Option<(PathBuf, PathBuf, Option<PathBuf>)>,
) -> Result<()> {
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("build ingress proxy client")?;
    let state = ProxyState {
        client,
        paths,
        ingress_scheme: if tls_paths.is_some() {
            "https".to_string()
        } else {
            "http".to_string()
        },
    };
    let app = Router::new()
        .fallback(any(proxy_request))
        .with_state(Arc::new(state));
    if let Some((cert_path, key_path, ca_path)) = tls_paths {
        let tls_config = tls::load_server_config(&cert_path, &key_path, ca_path.as_deref())?;
        let rustls_config =
            axum_server::tls_rustls::RustlsConfig::from_config(std::sync::Arc::new(tls_config));
        axum_server::bind_rustls(bind, rustls_config)
            .serve(app.into_make_service())
            .await?;
    } else {
        let listener = tokio::net::TcpListener::bind(bind).await?;
        serve_listener(listener, app).await?;
    }
    Ok(())
}

async fn serve_listener(listener: tokio::net::TcpListener, app: Router) -> Result<()> {
    axum::serve(listener, app).await?;
    Ok(())
}

async fn proxy_request(
    State(state): State<Arc<ProxyState>>,
    request: Request,
) -> Result<Response<Body>, StatusCode> {
    let host = request
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(|value| value.split(':').next().unwrap_or(value).to_string())
        .ok_or(StatusCode::BAD_REQUEST)?;

    let conn = state::open(&state.paths).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let routes = state::list_routes(&conn, None).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let route = routes
        .into_iter()
        .find(|route| route.domain == host)
        .ok_or(StatusCode::NOT_FOUND)?;
    let service = state::service_by_name(&conn, &route.service_name)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::BAD_GATEWAY)?;
    let sandbox = state::sandbox_by_service(&conn, &route.service_name)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if service.status != "healthy" && service.status != "degraded" {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let port = route.port.ok_or(StatusCode::BAD_GATEWAY)?;
    let target_host = service_target_host(sandbox.as_ref());

    let is_websocket = is_websocket_upgrade(request.headers());
    if is_websocket {
        let path_and_query = upstream_path(&conn, &route, &service, &host, request.uri())
            .map_err(map_ingress_error)?;
        return proxy_websocket(
            state.clone(),
            request,
            host,
            target_host.to_string(),
            port,
            path_and_query,
        )
        .await;
    }

    let (parts, body) = request.into_parts();
    let body_bytes = body
        .collect()
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?
        .to_bytes();
    let path_and_query =
        upstream_path(&conn, &route, &service, &host, &parts.uri).map_err(map_ingress_error)?;
    let target = format!("http://{target_host}:{port}{path_and_query}");
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let mut upstream = state.client.request(method, target).body(body_bytes);
    upstream = copy_headers(&parts.headers, upstream, &host, &state.ingress_scheme);
    let response = upstream.send().await.map_err(|_| StatusCode::BAD_GATEWAY)?;

    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .bytes()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    let mut out = Response::builder().status(status);
    for (name, value) in &headers {
        out = out.header(name, value);
    }
    out.body(Body::from(bytes))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn proxy_websocket(
    state: Arc<ProxyState>,
    mut request: Request,
    host: String,
    target_host: String,
    port: u16,
    path_and_query: String,
) -> Result<Response<Body>, StatusCode> {
    let sec_key = request
        .headers()
        .get("sec-websocket-key")
        .and_then(|value| value.to_str().ok())
        .ok_or(StatusCode::BAD_REQUEST)?
        .to_string();
    let response_key = websocket_accept_key(&sec_key);
    let protocol = request
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);

    let mut builder = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-accept", response_key);
    if let Some(protocol) = &protocol {
        builder = builder.header("sec-websocket-protocol", protocol);
    }

    let on_upgrade = hyper::upgrade::on(&mut request);
    let ingress_scheme = state.ingress_scheme.clone();
    tokio::spawn(async move {
        if let Ok(upgraded) = on_upgrade.await {
            let _ = relay_websocket(
                upgraded,
                host,
                target_host,
                port,
                path_and_query,
                ingress_scheme,
                protocol,
            )
            .await;
        }
    });

    builder
        .body(Body::empty())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn relay_websocket(
    upgraded: Upgraded,
    host: String,
    target_host: String,
    port: u16,
    path_and_query: String,
    ingress_scheme: String,
    protocol: Option<String>,
) -> Result<()> {
    let downstream = WebSocketStream::from_raw_socket(
        hyper_util::rt::TokioIo::new(upgraded),
        Role::Server,
        None,
    )
    .await;
    let target = format!("ws://{target_host}:{port}{path_and_query}");
    let mut upstream_request = target.into_client_request()?;
    upstream_request
        .headers_mut()
        .insert("x-forwarded-host", host.parse()?);
    upstream_request
        .headers_mut()
        .insert("x-forwarded-proto", ingress_scheme.parse()?);
    if let Some(protocol) = &protocol {
        upstream_request
            .headers_mut()
            .insert("sec-websocket-protocol", protocol.parse()?);
    }
    let (upstream, _) = connect_async(upstream_request).await?;
    tunnel_websockets(downstream, upstream).await
}

async fn tunnel_websockets(
    downstream: WebSocketStream<hyper_util::rt::TokioIo<Upgraded>>,
    upstream: WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
) -> Result<()> {
    let (mut downstream_write, mut downstream_read) = downstream.split();
    let (mut upstream_write, mut upstream_read) = upstream.split();

    let client_to_upstream = async {
        while let Some(message) = downstream_read.next().await {
            let message = message?;
            upstream_write.send(message).await?;
        }
        upstream_write.close().await?;
        Result::<()>::Ok(())
    };

    let upstream_to_client = async {
        while let Some(message) = upstream_read.next().await {
            let message: Message = message?;
            downstream_write.send(message).await?;
        }
        downstream_write.close().await?;
        Result::<()>::Ok(())
    };

    tokio::select! {
        result = client_to_upstream => result,
        result = upstream_to_client => result,
    }
}

fn copy_headers(
    headers: &HeaderMap,
    mut request: reqwest::RequestBuilder,
    host: &str,
    ingress_scheme: &str,
) -> reqwest::RequestBuilder {
    for (name, value) in headers {
        let lower = name.as_str();
        if lower.eq_ignore_ascii_case("host")
            || lower.eq_ignore_ascii_case("connection")
            || lower.eq_ignore_ascii_case("keep-alive")
            || lower.eq_ignore_ascii_case("proxy-authenticate")
            || lower.eq_ignore_ascii_case("proxy-authorization")
            || lower.eq_ignore_ascii_case("te")
            || lower.eq_ignore_ascii_case("trailer")
            || lower.eq_ignore_ascii_case("transfer-encoding")
            || lower.eq_ignore_ascii_case("upgrade")
        {
            continue;
        }
        request = request.header(name, value);
    }
    request = request.header("x-forwarded-host", host);
    request = request.header("x-forwarded-proto", ingress_scheme);
    request
}

fn upstream_path(
    conn: &rusqlite::Connection,
    route: &crate::model::RouteRecord,
    _service: &crate::model::ServiceRecord,
    host: &str,
    uri: &axum::http::Uri,
) -> anyhow::Result<String> {
    let path = uri.path();
    if path == "/_megaserver/signed" {
        let secret = state::secret_value(conn, &route.service_name, "MEGASERVER_SIGNING_KEY")?
            .ok_or_else(|| anyhow::anyhow!("signed links are not enabled for this service"))?;
        return crate::ingress::resolve_signed_target(
            &secret,
            host,
            &route.service_name,
            uri.query(),
        );
    }
    Ok(uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/")
        .to_string())
}

fn map_ingress_error(err: anyhow::Error) -> StatusCode {
    let message = err.to_string();
    if message.contains("expired")
        || message.contains("signature")
        || message.contains("enabled")
        || message.contains("missing signed-link")
    {
        return StatusCode::UNAUTHORIZED;
    }
    StatusCode::BAD_REQUEST
}

fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    let upgrade = headers
        .get("upgrade")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    let connection = headers
        .get("connection")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase().contains("upgrade"))
        .unwrap_or(false);
    upgrade && connection
}

fn websocket_accept_key(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{self, StatePaths};
    use crate::test_support::TEST_LOCK;
    use futures_util::{SinkExt, StreamExt};
    use tempfile::TempDir;
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::{accept_async, connect_async};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ingress_proxies_websockets() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let temp = TempDir::new().unwrap();
        let paths = StatePaths::resolve(Some(temp.path().join("proxy-home"))).unwrap();
        state::init(&paths).unwrap();

        let upstream_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let upstream_port = upstream_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (stream, _) = upstream_listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            if let Some(Ok(Message::Text(text))) = ws.next().await {
                ws.send(Message::Text(format!("echo:{text}").into()))
                    .await
                    .unwrap();
            }
        });

        let conn = state::open(&paths).unwrap();
        state::upsert_service(
            &conn,
            "ws-service",
            "healthy",
            temp.path(),
            "{}",
            "{}",
            Some(upstream_port),
            None,
        )
        .unwrap();
        state::put_route(&conn, "ws-service", "ws.local", Some(upstream_port)).unwrap();
        drop(conn);

        let proxy_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let proxy_port = proxy_listener.local_addr().unwrap().port();
        let proxy_paths = paths.clone();
        tokio::spawn(async move {
            let state = ProxyState {
                client: Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .unwrap(),
                paths: proxy_paths,
                ingress_scheme: "http".to_string(),
            };
            let app = Router::new()
                .fallback(any(proxy_request))
                .with_state(Arc::new(state));
            let _ = serve_listener(proxy_listener, app).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let mut request = format!("ws://127.0.0.1:{proxy_port}/socket")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("host", "ws.local".parse().unwrap());
        let (mut client, _) = connect_async(request).await.unwrap();
        client.send(Message::Text("ping".into())).await.unwrap();
        let reply = client.next().await.unwrap().unwrap();
        assert_eq!(reply.into_text().unwrap(), "echo:ping");
    }
}

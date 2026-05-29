use crate::cli::DaemonArgs;
use crate::controlplane;
use crate::proxy;
use crate::state::{self, StatePaths};
use crate::tls;
use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct DaemonState {
    pub paths: StatePaths,
}

pub async fn serve(paths: StatePaths, args: DaemonArgs) -> Result<()> {
    let state = Arc::new(DaemonState {
        paths: paths.clone(),
    });

    tokio::spawn(reconcile_loop(state.clone()));

    let ingress_bind: SocketAddr = args.ingress_bind.parse().context("invalid ingress bind")?;
    let proxy_paths = paths.clone();
    let ingress_tls = args
        .tls_cert
        .clone()
        .zip(args.tls_key.clone())
        .map(|(cert, key)| (cert, key, args.tls_ca.clone()));
    tokio::spawn(async move {
        let _ = proxy::serve(proxy_paths, ingress_bind, ingress_tls).await;
    });

    let app = router(state);
    let bind: SocketAddr = args.bind.parse().context("invalid daemon bind")?;
    if let (Some(cert_path), Some(key_path)) = (&args.tls_cert, &args.tls_key) {
        let tls_config = tls::load_server_config(cert_path, key_path, args.tls_ca.as_deref())?;
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

fn router(state: Arc<DaemonState>) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/services", get(list_services))
        .route("/v1/services/deploy", post(deploy))
        .route("/v1/services/{service}/start", post(start_service))
        .route("/v1/services/{service}/stop", post(stop_service))
        .route("/v1/services/{service}/restart", post(restart_service))
        .route("/v1/services/{service}/destroy", post(destroy_service))
        .route("/v1/services/{service}/inspect", get(inspect_service))
        .route("/v1/services/{service}/logs", get(logs_service))
        .route("/v1/services/{service}/shell", post(shell_service))
        .route("/v1/services/{service}/snapshot", post(snapshot_service))
        .route(
            "/v1/services/{service}/rollback/{snapshot}",
            post(rollback_service),
        )
        .route("/v1/routes", get(list_routes).post(create_route))
        .route("/v1/routes/signed-link", post(create_signed_link))
        .route("/v1/events", get(list_events))
        .route("/v1/volumes", get(list_volumes).post(create_volume))
        .route("/v1/secrets", post(set_secret).get(list_secrets))
        .with_state(state)
}

async fn health(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let (service_count, sandbox_count, route_count) =
        match state::open(&state.paths).and_then(|conn| {
            Ok((
                state::list_services(&conn)?.len(),
                state::list_sandboxes(&conn)?.len(),
                state::list_routes(&conn, None)?.len(),
            ))
        }) {
            Ok(counts) => counts,
            Err(_) => (0, 0, 0),
        };
    Json(json!({
        "status":"ok",
        "ingress":"running",
        "route_runtime_count":route_count,
        "route_count": route_count,
        "service_count": service_count,
        "sandbox_count": sandbox_count
    }))
}

#[derive(Debug, Deserialize)]
struct DeployRequest {
    app_path: String,
}

async fn deploy(
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<DeployRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let body = json!({"app_path": req.app_path});
    let value = controlplane::dispatch_http_ok(
        &state.paths.home,
        "POST",
        "/v1/services/deploy",
        Some(&body),
    )
    .map_err(internal_error)?;
    Ok(Json(value))
}

async fn list_services(
    State(state): State<Arc<DaemonState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let value = controlplane::dispatch_http_ok(&state.paths.home, "GET", "/v1/services", None)
        .map_err(internal_error)?;
    Ok(Json(value))
}

async fn start_service(
    State(state): State<Arc<DaemonState>>,
    Path(service): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = format!("/v1/services/{service}/start");
    let value = controlplane::dispatch_http_ok(&state.paths.home, "POST", &path, None)
        .map_err(internal_error)?;
    Ok(Json(value))
}

async fn stop_service(
    State(state): State<Arc<DaemonState>>,
    Path(service): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = format!("/v1/services/{service}/stop");
    let value = controlplane::dispatch_http_ok(&state.paths.home, "POST", &path, None)
        .map_err(internal_error)?;
    Ok(Json(value))
}

async fn restart_service(
    State(state): State<Arc<DaemonState>>,
    Path(service): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = format!("/v1/services/{service}/restart");
    let value = controlplane::dispatch_http_ok(&state.paths.home, "POST", &path, None)
        .map_err(internal_error)?;
    Ok(Json(value))
}

async fn destroy_service(
    State(state): State<Arc<DaemonState>>,
    Path(service): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = format!("/v1/services/{service}/destroy");
    let value = controlplane::dispatch_http_ok(&state.paths.home, "POST", &path, None)
        .map_err(internal_error)?;
    Ok(Json(value))
}

async fn inspect_service(
    State(state): State<Arc<DaemonState>>,
    Path(service): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = format!("/v1/services/{service}/inspect");
    let value = controlplane::dispatch_http_ok(&state.paths.home, "GET", &path, None)
        .map_err(internal_error)?;
    Ok(Json(value))
}

#[derive(Debug, Deserialize)]
struct LogsQuery {
    lines: Option<usize>,
}

async fn logs_service(
    State(state): State<Arc<DaemonState>>,
    Path(service): Path<String>,
    Query(query): Query<LogsQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = format!(
        "/v1/services/{service}/logs?lines={}",
        query.lines.unwrap_or(50)
    );
    let value = controlplane::dispatch_http_ok(&state.paths.home, "GET", &path, None)
        .map_err(internal_error)?;
    Ok(Json(value))
}

#[derive(Debug, Deserialize)]
struct ShellRequest {
    command: Option<Vec<String>>,
}

async fn shell_service(
    State(state): State<Arc<DaemonState>>,
    Path(service): Path<String>,
    Json(req): Json<ShellRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let command = req
        .command
        .unwrap_or_default()
        .into_iter()
        .map(Value::String)
        .collect::<Vec<_>>();
    let body = json!({"command": command});
    let path = format!("/v1/services/{service}/shell");
    let value = controlplane::dispatch_http_ok(&state.paths.home, "POST", &path, Some(&body))
        .map_err(internal_error)?;
    Ok(Json(value))
}

async fn snapshot_service(
    State(state): State<Arc<DaemonState>>,
    Path(service): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = format!("/v1/services/{service}/snapshot");
    let value = controlplane::dispatch_http_ok(&state.paths.home, "POST", &path, None)
        .map_err(internal_error)?;
    Ok(Json(value))
}

async fn rollback_service(
    State(state): State<Arc<DaemonState>>,
    Path((service, snapshot)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = format!("/v1/services/{service}/rollback/{snapshot}");
    let value = controlplane::dispatch_http_ok(&state.paths.home, "POST", &path, None)
        .map_err(internal_error)?;
    Ok(Json(value))
}

#[derive(Debug, Deserialize)]
struct RouteRequest {
    service: String,
    domain: String,
    port: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct SignedLinkRequest {
    service: String,
    domain: String,
    path: String,
    expires_in: Option<u64>,
    scheme: Option<String>,
}

async fn create_route(
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<RouteRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let body = json!({
        "service": req.service,
        "domain": req.domain,
        "port": req.port
    });
    let value =
        controlplane::dispatch_http_ok(&state.paths.home, "POST", "/v1/routes", Some(&body))
            .map_err(internal_error)?;
    Ok(Json(value))
}

async fn create_signed_link(
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<SignedLinkRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let body = json!({
        "service": req.service,
        "domain": req.domain,
        "path": req.path,
        "expires_in": req.expires_in.unwrap_or(300),
        "scheme": req.scheme.unwrap_or_else(|| "https".to_string())
    });
    let value = controlplane::dispatch_http_ok(
        &state.paths.home,
        "POST",
        "/v1/routes/signed-link",
        Some(&body),
    )
    .map_err(internal_error)?;
    Ok(Json(value))
}

async fn list_routes(
    State(state): State<Arc<DaemonState>>,
    Query(query): Query<SecretQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = if let Some(service) = query.service {
        format!("/v1/routes?service={service}")
    } else {
        "/v1/routes".to_string()
    };
    let value = controlplane::dispatch_http_ok(&state.paths.home, "GET", &path, None)
        .map_err(internal_error)?;
    Ok(Json(value))
}

#[derive(Debug, Deserialize)]
struct VolumeRequest {
    name: String,
    service: Option<String>,
}

async fn create_volume(
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<VolumeRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let body = json!({
        "name": req.name,
        "service": req.service
    });
    let value =
        controlplane::dispatch_http_ok(&state.paths.home, "POST", "/v1/volumes", Some(&body))
            .map_err(internal_error)?;
    Ok(Json(value))
}

async fn list_volumes(
    State(state): State<Arc<DaemonState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let value = controlplane::dispatch_http_ok(&state.paths.home, "GET", "/v1/volumes", None)
        .map_err(internal_error)?;
    Ok(Json(value))
}

#[derive(Debug, Deserialize)]
struct SecretRequest {
    service: String,
    key: String,
    value: String,
}

async fn set_secret(
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<SecretRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let body = json!({
        "service": req.service,
        "key": req.key,
        "value": req.value
    });
    let value =
        controlplane::dispatch_http_ok(&state.paths.home, "POST", "/v1/secrets", Some(&body))
            .map_err(internal_error)?;
    Ok(Json(value))
}

#[derive(Debug, Deserialize)]
struct SecretQuery {
    service: Option<String>,
}

async fn list_secrets(
    State(state): State<Arc<DaemonState>>,
    Query(query): Query<SecretQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = if let Some(service) = query.service {
        format!("/v1/secrets?service={service}")
    } else {
        "/v1/secrets".to_string()
    };
    let value = controlplane::dispatch_http_ok(&state.paths.home, "GET", &path, None)
        .map_err(internal_error)?;
    Ok(Json(value))
}

#[derive(Debug, Deserialize)]
struct EventQuery {
    service: Option<String>,
}

async fn list_events(
    State(state): State<Arc<DaemonState>>,
    Query(query): Query<EventQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = if let Some(service) = query.service {
        format!("/v1/events?service={service}")
    } else {
        "/v1/events".to_string()
    };
    let value = controlplane::dispatch_http_ok(&state.paths.home, "GET", &path, None)
        .map_err(internal_error)?;
    Ok(Json(value))
}

async fn reconcile_loop(state: Arc<DaemonState>) {
    loop {
        if let Err(err) = reconcile_once(&state).await {
            eprintln!("megaserver reconcile error: {err:#}");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn reconcile_once(state: &Arc<DaemonState>) -> Result<()> {
    let conn = state::open(&state.paths)?;
    let services = state::list_services(&conn)?;
    let sandboxes = state::list_sandboxes(&conn)?;
    drop(conn);
    for service in services {
        if let Some(pid) = service.pid {
            let alive = crate::runtime::process_alive(pid);
            let conn = state::open(&state.paths)?;
            if !alive && service.status != "stopped" {
                if let Some(sandbox) = state::sandbox_by_service(&conn, &service.name)? {
                    crate::sandbox::cleanup_sandbox(&crate::sandbox::SandboxLaunchMetadata {
                        service_name: Some(service.name.clone()),
                        runtime_kind: sandbox.runtime_kind.clone(),
                        isolation_mode: sandbox.isolation_mode.clone(),
                        cgroup_path: sandbox.cgroup_path.clone(),
                    })?;
                    state::upsert_sandbox(
                        &conn,
                        state::SandboxUpsert {
                            service_name: &service.name,
                            sandbox_id: &sandbox.sandbox_id,
                            hostname: &sandbox.hostname,
                            ip_address: sandbox.ip_address.as_deref(),
                            runtime_kind: &sandbox.runtime_kind,
                            isolation_mode: &sandbox.isolation_mode,
                            status: "stopped",
                            pid: None,
                            cgroup_path: None,
                        },
                    )?;
                }
                state::update_service_status(&conn, &service.name, "failed", None)?;
                state::emit_event(
                    &conn,
                    Some(&service.name),
                    "service.failed",
                    json!({"pid": pid, "reason": "process_exited"}),
                )?;
                continue;
            }
            if alive && service.port.is_some() {
                let sandbox = state::sandbox_by_service(&conn, &service.name)?;
                let target_host = match sandbox.as_ref() {
                    Some(sandbox) if sandbox.runtime_kind == "linux-namespace" => {
                        sandbox.ip_address.as_deref().unwrap_or("127.0.0.1")
                    }
                    _ => "127.0.0.1",
                };
                let health = crate::runtime::health_check(
                    target_host,
                    service.port,
                    service.health_path.as_deref(),
                );
                let next = if health.is_ok() {
                    "healthy"
                } else {
                    "degraded"
                };
                if next != service.status {
                    state::update_service_status(&conn, &service.name, next, Some(pid))?;
                }
            }
        }
    }
    for sandbox in sandboxes {
        let stale = sandbox
            .pid
            .is_none_or(|pid| !crate::runtime::process_alive(pid));
        if sandbox.runtime_kind == "linux-namespace" && stale && sandbox.status != "stopped" {
            let conn = state::open(&state.paths)?;
            crate::sandbox::cleanup_sandbox(&crate::sandbox::SandboxLaunchMetadata {
                service_name: Some(sandbox.service_name.clone()),
                runtime_kind: sandbox.runtime_kind.clone(),
                isolation_mode: sandbox.isolation_mode.clone(),
                cgroup_path: sandbox.cgroup_path.clone(),
            })?;
            state::upsert_sandbox(
                &conn,
                state::SandboxUpsert {
                    service_name: &sandbox.service_name,
                    sandbox_id: &sandbox.sandbox_id,
                    hostname: &sandbox.hostname,
                    ip_address: sandbox.ip_address.as_deref(),
                    runtime_kind: &sandbox.runtime_kind,
                    isolation_mode: &sandbox.isolation_mode,
                    status: "stopped",
                    pid: None,
                    cgroup_path: None,
                },
            )?;
        }
    }
    crate::dns::stop_if_idle(&state.paths)?;
    Ok(())
}

fn internal_error(err: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::DaemonArgs;
    use crate::test_support::TEST_LOCK;
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
    use reqwest::Certificate;
    use reqwest::Client;
    use serde_json::json;
    use std::fs;
    use std::net::IpAddr;
    use std::net::TcpListener;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .expect("bind ephemeral port")
            .local_addr()
            .expect("local addr")
            .port()
    }

    fn write_test_certificates(dir: &TempDir) -> (PathBuf, PathBuf, Certificate) {
        let mut params =
            CertificateParams::new(vec!["localhost".to_string(), "hello.local".to_string()])
                .unwrap();
        params
            .subject_alt_names
            .push(SanType::IpAddress(IpAddr::from([127, 0, 0, 1])));
        let mut distinguished = DistinguishedName::new();
        distinguished.push(DnType::CommonName, "megaserver-test");
        params.distinguished_name = distinguished;
        let key_pair = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key_pair).unwrap();
        let cert_pem = cert.pem();
        let key_pem = key_pair.serialize_pem();

        let cert_path = dir.path().join("server.pem");
        let key_path = dir.path().join("server.key");
        fs::write(&cert_path, cert_pem.as_bytes()).unwrap();
        fs::write(&key_path, key_pem.as_bytes()).unwrap();

        let root = Certificate::from_pem(cert_pem.as_bytes()).unwrap();
        (cert_path, key_path, root)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn daemon_serves_api_and_ingress_end_to_end() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let temp = TempDir::new().unwrap();
        let paths = StatePaths::resolve(Some(temp.path().join("daemon-home"))).unwrap();
        state::init(&paths).unwrap();

        let bind = format!("127.0.0.1:{}", free_port());
        let ingress_bind = format!("127.0.0.1:{}", free_port());
        let args = DaemonArgs {
            bind: bind.clone(),
            ingress_bind: ingress_bind.clone(),
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
        };

        let serve_paths = paths.clone();
        let server = tokio::spawn(async move { serve(serve_paths, args).await });

        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let example = repo_root.join("examples/hello-service");

        let mut healthy = false;
        for _ in 0..50 {
            if let Ok(response) = client.get(format!("http://{bind}/v1/health")).send().await
                && response.status().is_success()
            {
                healthy = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(healthy, "daemon health endpoint never became ready");

        let deploy = client
            .post(format!("http://{bind}/v1/services/deploy"))
            .json(&json!({"app_path": example.display().to_string()}))
            .send()
            .await
            .unwrap();
        assert!(deploy.status().is_success());
        let deploy_json: Value = deploy.json().await.unwrap();
        assert_eq!(
            deploy_json["service"]["name"].as_str(),
            Some("hello-service")
        );

        let secret = client
            .post(format!("http://{bind}/v1/secrets"))
            .json(&json!({"service":"hello-service","key":"API_KEY","value":"s3cr3t"}))
            .send()
            .await
            .unwrap();
        assert!(secret.status().is_success());
        let signing_secret = client
            .post(format!("http://{bind}/v1/secrets"))
            .json(
                &json!({"service":"hello-service","key":"MEGASERVER_SIGNING_KEY","value":"sign-secret"}),
            )
            .send()
            .await
            .unwrap();
        assert!(signing_secret.status().is_success());

        let shell = client
            .post(format!("http://{bind}/v1/services/hello-service/shell"))
            .json(&json!({
                "command": ["python3", "-c", "import os; assert os.environ['API_KEY']=='s3cr3t'; assert os.environ['MEGASERVER_SANDBOX_HOSTNAME']=='hello-service.sandbox.megaserver'"]
            }))
            .send()
            .await
            .unwrap();
        assert!(shell.status().is_success());

        let ingress = client
            .get(format!("http://{ingress_bind}/health"))
            .header("host", "hello.local")
            .send()
            .await
            .unwrap();
        assert!(ingress.status().is_success());
        assert_eq!(ingress.text().await.unwrap(), "ok\n");

        let routes = client
            .get(format!("http://{bind}/v1/routes?service=hello-service"))
            .send()
            .await
            .unwrap();
        assert!(routes.status().is_success());
        let routes_json: Value = routes.json().await.unwrap();
        assert_eq!(routes_json.as_array().map(Vec::len), Some(1));

        let signed_link = client
            .post(format!("http://{bind}/v1/routes/signed-link"))
            .json(&json!({
                "service":"hello-service",
                "domain":"hello.local",
                "path":"/private/check?one=1",
                "expires_in":300,
                "scheme":"http"
            }))
            .send()
            .await
            .unwrap();
        assert!(signed_link.status().is_success());
        let signed_link_json: Value = signed_link.json().await.unwrap();
        let ingress_path = signed_link_json["ingress_path"].as_str().unwrap();

        let signed_response = client
            .get(format!("http://{ingress_bind}{ingress_path}"))
            .header("host", "hello.local")
            .send()
            .await
            .unwrap();
        assert!(signed_response.status().is_success());
        assert!(
            signed_response
                .text()
                .await
                .unwrap()
                .contains("hello from hello-service")
        );

        let destroy = client
            .post(format!("http://{bind}/v1/services/hello-service/destroy"))
            .send()
            .await
            .unwrap();
        assert!(destroy.status().is_success());

        server.abort();
        let _ = server.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn daemon_serves_https_for_api_and_ingress() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let temp = TempDir::new().unwrap();
        let paths = StatePaths::resolve(Some(temp.path().join("daemon-tls-home"))).unwrap();
        state::init(&paths).unwrap();
        let (cert_path, key_path, root_cert) = write_test_certificates(&temp);

        let bind_port = free_port();
        let ingress_port = free_port();
        let bind = format!("127.0.0.1:{bind_port}");
        let ingress_bind = format!("127.0.0.1:{ingress_port}");
        let args = DaemonArgs {
            bind: bind.clone(),
            ingress_bind: ingress_bind.clone(),
            tls_cert: Some(cert_path),
            tls_key: Some(key_path),
            tls_ca: None,
        };

        let serve_paths = paths.clone();
        let server = tokio::spawn(async move { serve(serve_paths, args).await });

        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .add_root_certificate(root_cert)
            .build()
            .unwrap();

        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let example = repo_root.join("examples/hello-service");

        let mut healthy = false;
        for _ in 0..50 {
            if let Ok(response) = client
                .get(format!("https://localhost:{bind_port}/v1/health"))
                .send()
                .await
                && response.status().is_success()
            {
                healthy = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(healthy, "daemon TLS health endpoint never became ready");

        let deploy = client
            .post(format!("https://localhost:{bind_port}/v1/services/deploy"))
            .json(&json!({"app_path": example.display().to_string()}))
            .send()
            .await
            .unwrap();
        assert!(deploy.status().is_success());

        let signing_secret = client
            .post(format!("https://localhost:{bind_port}/v1/secrets"))
            .json(
                &json!({"service":"hello-service","key":"MEGASERVER_SIGNING_KEY","value":"sign-secret"}),
            )
            .send()
            .await
            .unwrap();
        assert!(signing_secret.status().is_success());

        let signed_link = client
            .post(format!(
                "https://localhost:{bind_port}/v1/routes/signed-link"
            ))
            .json(&json!({
                "service":"hello-service",
                "domain":"hello.local",
                "path":"/tls/check",
                "expires_in":300,
                "scheme":"https"
            }))
            .send()
            .await
            .unwrap();
        assert!(signed_link.status().is_success());
        let signed_link_json: Value = signed_link.json().await.unwrap();
        let ingress_path = signed_link_json["ingress_path"].as_str().unwrap();

        let ingress = client
            .get(format!("https://localhost:{ingress_port}{ingress_path}"))
            .header("host", "hello.local")
            .send()
            .await
            .unwrap();
        assert!(ingress.status().is_success());
        assert!(
            ingress
                .text()
                .await
                .unwrap()
                .contains("hello from hello-service")
        );

        let destroy = client
            .post(format!(
                "https://localhost:{bind_port}/v1/services/hello-service/destroy"
            ))
            .send()
            .await
            .unwrap();
        assert!(destroy.status().is_success());

        server.abort();
        let _ = server.await;
    }
}

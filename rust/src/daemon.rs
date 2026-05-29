use crate::cli::DaemonArgs;
use crate::http_host;
use crate::proxy;
use crate::state::{self, StatePaths};
use anyhow::{Context, Result};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

#[derive(Clone)]
pub struct DaemonState {
    pub paths: StatePaths,
}

struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub async fn serve(paths: StatePaths, args: DaemonArgs) -> Result<()> {
    let state = Arc::new(DaemonState {
        paths: paths.clone(),
    });

    let _reconcile = AbortOnDrop(tokio::spawn(reconcile_loop(state.clone())));

    let ingress_bind: SocketAddr = args.ingress_bind.parse().context("invalid ingress bind")?;
    let proxy_paths = paths.clone();
    let ingress_tls = args
        .tls_cert
        .clone()
        .zip(args.tls_key.clone())
        .map(|(cert, key)| (cert, key, args.tls_ca.clone()));
    let _proxy = AbortOnDrop(tokio::spawn(async move {
        let _ = proxy::serve(proxy_paths, ingress_bind, ingress_tls).await;
    }));

    let bind: SocketAddr = args.bind.parse().context("invalid daemon bind")?;
    let control_tls = args
        .tls_cert
        .clone()
        .zip(args.tls_key.clone())
        .map(|(cert, key)| (cert, key, args.tls_ca.clone()));
    http_host::serve_fzy_control_plane(paths.home.clone(), bind, control_tls).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app;
    use crate::cli::DaemonArgs;
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
    use reqwest::Certificate;
    use reqwest::Client;
    use serde_json::{Value, json};
    use std::fs;
    use std::net::IpAddr;
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::process::Command;
    use std::thread;
    use std::time::Duration;
    use tempfile::TempDir;

    fn cleanup_hello_service(repo_root: &std::path::Path) {
        let target = repo_root.join("examples/hello-service/server.py");
        let _ = Command::new("pkill")
            .args(["-f", target.to_string_lossy().as_ref()])
            .status();
        thread::sleep(Duration::from_millis(150));
    }
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

    #[cfg(target_os = "linux")]
    fn linux_isolation_supported() -> bool {
        crate::network::linux::isolation_supported()
    }

    #[cfg(not(target_os = "linux"))]
    fn linux_isolation_supported() -> bool {
        false
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn daemon_serves_api_and_ingress_end_to_end() {
        let _guard = crate::test_support::INTEGRATION_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        crate::test_support::cleanup_fzy_io_files();
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
        cleanup_hello_service(&repo_root);
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

        if linux_isolation_supported() {
            let inspect = client
                .get(format!("http://{bind}/v1/services/hello-service/inspect"))
                .send()
                .await
                .unwrap();
            assert!(inspect.status().is_success());
            let inspect_json: Value = inspect.json().await.unwrap();
            assert_eq!(
                inspect_json["sandbox"]["runtime_kind"].as_str(),
                Some("linux-namespace")
            );
            assert!(
                inspect_json["sandbox"]["isolation_mode"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("pid")
            );

            let shell = client
                .post(format!("http://{bind}/v1/services/hello-service/shell"))
                .json(&json!({
                    "command": ["python3", "-c", "from pathlib import Path; import os; assert Path.cwd() == Path('/srv/app'); volume = Path('/srv/app/.megaserver/volumes/hello-data'); test_file = volume / 'daemon-marker.txt'; test_file.write_text('daemon'); assert test_file.read_text() == 'daemon'; failed = False\ntry:\n    Path('/srv/app/server.py').write_text('mutated')\nexcept OSError:\n    failed = True\nassert failed"]
                }))
                .send()
                .await
                .unwrap();
            assert!(shell.status().is_success());
            assert_eq!(
                fs::read_to_string(
                    paths
                        .volumes_dir
                        .join("hello-data")
                        .join("daemon-marker.txt")
                )
                .unwrap(),
                "daemon"
            );
        }

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
        let routes_status = routes.status();
        let routes_body = routes.text().await.unwrap();
        assert!(
            routes_status.is_success(),
            "routes failed with status {routes_status}: {routes_body}"
        );
        let routes_json: Value = serde_json::from_str(&routes_body).unwrap();
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
        let destroy_status = destroy.status();
        let destroy_body = destroy.text().await.unwrap();
        assert!(
            destroy_status.is_success(),
            "destroy failed with status {destroy_status}: {destroy_body}"
        );

        server.abort();
        let _ = server.await;
        cleanup_hello_service(&repo_root);
        crate::test_support::cleanup_fzy_io_files();
    }

    // The separate-process Fozzy TLS scenario is the authoritative coverage for this path.
    // This in-process variant reuses the file-backed FZY host ABI and can flake after another
    // daemon test even when the real daemon/TLS path is healthy.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "covered deterministically by tests/megaserver.daemon.tls.pass.fozzy.json"]
    async fn daemon_serves_https_for_api_and_ingress() {
        let _guard = crate::test_support::INTEGRATION_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        crate::test_support::cleanup_fzy_io_files();
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
        cleanup_hello_service(&repo_root);
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
        let signing_secret_status = signing_secret.status();
        let signing_secret_body = signing_secret.text().await.unwrap();
        assert!(
            signing_secret_status.is_success(),
            "signing secret failed with status {signing_secret_status}: {signing_secret_body}"
        );

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
        let destroy_status = destroy.status();
        let destroy_body = destroy.text().await.unwrap();
        assert!(
            destroy_status.is_success(),
            "destroy failed with status {destroy_status}: {destroy_body}"
        );

        server.abort();
        let _ = server.await;
        cleanup_hello_service(&repo_root);
        crate::test_support::cleanup_fzy_io_files();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconcile_cleans_up_crashed_linux_sandbox_resources() {
        if !linux_isolation_supported() {
            return;
        }

        let _guard = crate::test_support::INTEGRATION_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        crate::test_support::cleanup_fzy_io_files();
        let temp = TempDir::new().unwrap();
        let paths = StatePaths::resolve(Some(temp.path().join("reconcile-home"))).unwrap();
        state::init(&paths).unwrap();

        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let example = repo_root.join("examples/hello-service");

        app::deploy_and_start_only(&paths, &example).unwrap();
        let conn = state::open(&paths).unwrap();
        let service = state::service_by_name(&conn, "hello-service")
            .unwrap()
            .expect("service record");
        let sandbox = state::sandbox_by_service(&conn, "hello-service")
            .unwrap()
            .expect("sandbox record");
        drop(conn);

        let pid = service.pid.expect("running pid");
        assert!(crate::runtime::process_alive(pid));
        assert!(
            crate::network::linux::sandbox_network_present("hello-service").unwrap(),
            "sandbox network should exist before crash"
        );

        crate::runtime::stop_pid(pid, Duration::from_secs(2)).unwrap();
        let daemon_state = Arc::new(DaemonState {
            paths: paths.clone(),
        });
        reconcile_once(&daemon_state).await.unwrap();

        let conn = state::open(&paths).unwrap();
        let service = state::service_by_name(&conn, "hello-service")
            .unwrap()
            .expect("service record after reconcile");
        let sandbox_after = state::sandbox_by_service(&conn, "hello-service")
            .unwrap()
            .expect("sandbox record after reconcile");
        assert_eq!(service.status, "failed");
        assert_eq!(sandbox_after.status, "stopped");
        assert_eq!(sandbox_after.pid, None);
        assert_eq!(sandbox_after.cgroup_path, None);
        drop(conn);

        assert!(
            !crate::network::linux::sandbox_network_present("hello-service").unwrap(),
            "sandbox network should be cleaned up after reconcile"
        );
        if let Some(cgroup_path) = sandbox.cgroup_path {
            assert!(
                !std::path::Path::new(&cgroup_path).exists(),
                "cgroup path should be removed after reconcile"
            );
        }

        app::destroy_only(&paths, "hello-service").unwrap();
    }
}

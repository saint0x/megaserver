use crate::cli::{Cli, Commands, RouteCommands, SecretCommands, VolumeCommands};
use crate::controlplane;
use crate::daemon;
use crate::ingress;
use crate::manifest::{ServiceManifest, load_manifest};
use crate::planner;
use crate::runtime;
use crate::state::{self, StatePaths};
use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = StatePaths::resolve(cli.home)?;
    state::init(&paths)?;

    match cli.command {
        Commands::Init => cmd_init(&paths),
        Commands::Deploy(args) => cmd_deploy(&paths, &args.app_path),
        Commands::Start(args) => cmd_start(&paths, &args.service),
        Commands::Stop(args) => cmd_stop(&paths, &args.service),
        Commands::Restart(args) => {
            cmd_stop(&paths, &args.service)?;
            cmd_start(&paths, &args.service)
        }
        Commands::Destroy(args) => cmd_destroy(&paths, &args.service),
        Commands::Ps | Commands::Services => cmd_services(&paths),
        Commands::Logs(args) => cmd_logs(&paths, &args.service, args.lines),
        Commands::Route(args) => cmd_route(&paths, args.command),
        Commands::Expose(args) => cmd_expose(&paths, &args.service, &args.domain, args.port),
        Commands::Volumes(args) => cmd_volumes(&paths, args.command),
        Commands::Secrets(args) => cmd_secrets(&paths, args.command),
        Commands::Snapshot(args) => cmd_snapshot(&paths, &args.service),
        Commands::Rollback(args) => cmd_rollback(&paths, &args.service, &args.snapshot),
        Commands::Inspect(args) => cmd_inspect(&paths, &args.service),
        Commands::Events(args) => cmd_events(&paths, args.service.as_deref()),
        Commands::Shell(args) => cmd_shell(&paths, &args.service, &args.command),
        Commands::Daemon(args) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(daemon::serve(paths, args))
        }
    }
}

fn cmd_init(paths: &StatePaths) -> Result<()> {
    println!(
        "{}",
        json!({
            "status": "ok",
            "home": paths.home,
            "db": paths.db_path
        })
    );
    Ok(())
}

fn cmd_deploy(paths: &StatePaths, app_path: &Path) -> Result<()> {
    let value = controlplane::dispatch_ok(
        &paths.home,
        "deploy",
        &[(
            "app_path",
            Value::String(
                app_path
                    .canonicalize()
                    .with_context(|| format!("canonicalize {}", app_path.display()))?
                    .display()
                    .to_string(),
            ),
        )],
    )?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

pub fn deploy_and_start_only(paths: &StatePaths, app_path: &Path) -> Result<String> {
    deploy_only(paths, app_path)?;
    let (_manifest_path, manifest) = load_manifest(
        &app_path
            .canonicalize()
            .with_context(|| format!("canonicalize {}", app_path.display()))?,
    )?;
    start_only(paths, &manifest.name)?;
    Ok(manifest.name)
}

pub fn deploy_only(paths: &StatePaths, app_path: &Path) -> Result<()> {
    let app_path = app_path
        .canonicalize()
        .with_context(|| format!("canonicalize {}", app_path.display()))?;
    let (_manifest_path, manifest) = load_manifest(&app_path)?;
    let plan = planner::plan_manifest(&paths.home, &manifest)?;
    let manifest_json = serde_json::to_string_pretty(&manifest)?;
    let plan_json = serde_json::to_string_pretty(&plan)?;
    let conn = state::open(paths)?;

    state::upsert_service(
        &conn,
        &manifest.name,
        "deployed",
        &app_path,
        &manifest_json,
        &plan_json,
        manifest.network.port,
        manifest.health.path.as_deref(),
    )?;
    state::insert_deployment(&conn, &manifest.name, &manifest_json, &plan_json)?;
    for volume in &manifest.volumes {
        state::ensure_volume(&conn, paths, Some(&manifest.name), volume)?;
    }
    for route in &manifest.routes {
        state::put_route(&conn, &manifest.name, route, manifest.network.port)?;
    }
    state::emit_event(
        &conn,
        Some(&manifest.name),
        "deployment.created",
        json!({"app_path": app_path, "plan": plan}),
    )?;
    Ok(())
}

fn manifest_from_record(record: &crate::model::ServiceRecord) -> Result<ServiceManifest> {
    serde_json::from_str(&record.manifest_json).context("decode service manifest from sqlite state")
}

fn load_service(paths: &StatePaths, service: &str) -> Result<crate::model::ServiceRecord> {
    let conn = state::open(paths)?;
    state::service_by_name(&conn, service)?.ok_or_else(|| anyhow!("unknown service `{service}`"))
}

fn service_secret_env(paths: &StatePaths, service: &str) -> Result<Vec<(String, String)>> {
    let conn = state::open(paths)?;
    let secrets = state::list_secrets(&conn, Some(service))?;
    Ok(secrets
        .into_iter()
        .map(|secret| (secret.key, secret.value))
        .collect())
}

fn service_volume_env(
    paths: &StatePaths,
    service: &str,
    manifest: &ServiceManifest,
) -> Result<Vec<(String, String)>> {
    let conn = state::open(paths)?;
    let mut vars = Vec::new();
    for volume_name in &manifest.volumes {
        let volume = state::ensure_volume(&conn, paths, Some(service), volume_name)?;
        let key = format!(
            "MEGASERVER_VOLUME_{}",
            volume_name.replace('-', "_").to_ascii_uppercase()
        );
        vars.push((key, volume.host_path));
    }
    Ok(vars)
}

fn sandbox_hostname(service: &str) -> String {
    format!("{service}.sandbox.megaserver")
}

fn sandbox_id(service: &str, pid: i32) -> String {
    format!("{service}-{pid}")
}

fn pending_sandbox_id(service: &str) -> String {
    format!("{service}-pending")
}

fn sandbox_runtime_env(
    conn: &rusqlite::Connection,
    service: &str,
    sandbox_id_value: &str,
    sandbox_hostname_value: &str,
) -> Result<Vec<(String, String)>> {
    let sandbox_ip_value = state::allocate_sandbox_ip(conn, service)?;
    Ok(vec![
        (
            "MEGASERVER_SANDBOX_ID".to_string(),
            sandbox_id_value.to_string(),
        ),
        (
            "MEGASERVER_SANDBOX_HOSTNAME".to_string(),
            sandbox_hostname_value.to_string(),
        ),
        ("MEGASERVER_SANDBOX_IP".to_string(), sandbox_ip_value),
        ("HOSTNAME".to_string(), sandbox_hostname_value.to_string()),
    ])
}

fn cmd_start(paths: &StatePaths, service: &str) -> Result<()> {
    let value = controlplane::dispatch_ok(
        &paths.home,
        "start",
        &[("service", Value::String(service.to_string()))],
    )?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

pub fn start_only(paths: &StatePaths, service: &str) -> Result<()> {
    let record = load_service(paths, service)?;
    if let Some(pid) = record.pid
        && runtime::process_alive(pid)
    {
        bail!("service `{service}` is already running with pid {pid}");
    }

    let manifest = manifest_from_record(&record)?;
    let secret_env = service_secret_env(paths, service)?;
    let volume_env = service_volume_env(paths, service, &manifest)?;
    let conn = state::open(paths)?;
    let sandbox_hostname = sandbox_hostname(service);
    let sandbox_env = sandbox_runtime_env(
        &conn,
        service,
        &pending_sandbox_id(service),
        &sandbox_hostname,
    )?;
    let sandbox_ip_text = sandbox_env
        .iter()
        .find(|(key, _)| key == "MEGASERVER_SANDBOX_IP")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    let spawned = runtime::spawn_service(
        paths,
        service,
        Path::new(&record.app_path),
        &manifest,
        &secret_env,
        &volume_env,
        &sandbox_env,
    )?;
    let sandbox_id = sandbox_id(service, spawned.pid);
    state::update_service_status(&conn, service, "starting", Some(spawned.pid))?;
    state::upsert_sandbox(
        &conn,
        state::SandboxUpsert {
            service_name: service,
            sandbox_id: &sandbox_id,
            hostname: &sandbox_hostname,
            ip_address: Some(&sandbox_ip_text),
            runtime_kind: &spawned.runtime_kind,
            isolation_mode: &spawned.isolation_mode,
            status: "starting",
            pid: Some(spawned.pid),
            cgroup_path: spawned.cgroup_path.as_deref(),
        },
    )?;
    state::emit_event(
        &conn,
        Some(service),
        "service.started",
        json!({
            "pid": spawned.pid,
            "stdout_log": spawned.stdout_log,
            "stderr_log": spawned.stderr_log,
            "sandbox_id": sandbox_id,
            "hostname": sandbox_hostname,
            "ip_address": sandbox_ip_text.clone()
        }),
    )?;

    let health_status = match runtime::health_check(
        manifest.network.port,
        manifest.health.path.as_deref(),
    ) {
        Ok(ok) => ok,
        Err(err) => {
            state::update_service_status(&conn, service, "degraded", Some(spawned.pid))?;
            state::upsert_sandbox(
                &conn,
                state::SandboxUpsert {
                    service_name: service,
                    sandbox_id: &sandbox_id,
                    hostname: &sandbox_hostname,
                    ip_address: Some(&sandbox_ip_text),
                    runtime_kind: &spawned.runtime_kind,
                    isolation_mode: &spawned.isolation_mode,
                    status: "degraded",
                    pid: Some(spawned.pid),
                    cgroup_path: spawned.cgroup_path.as_deref(),
                },
            )?;
            state::emit_event(
                &conn,
                Some(service),
                "service.health.degraded",
                json!({"pid": spawned.pid, "error": err.to_string(), "sandbox_id": sandbox_id, "ip_address": sandbox_ip_text.clone(), "runtime_kind": spawned.runtime_kind, "isolation_mode": spawned.isolation_mode, "cgroup_path": spawned.cgroup_path}),
            )?;
            println!(
                "{}",
                json!({"status": "degraded", "service": service, "pid": spawned.pid, "error": err.to_string()})
            );
            return Ok(());
        }
    };

    state::update_service_status(&conn, service, "healthy", Some(spawned.pid))?;
    state::upsert_sandbox(
        &conn,
        state::SandboxUpsert {
            service_name: service,
            sandbox_id: &sandbox_id,
            hostname: &sandbox_hostname,
            ip_address: Some(&sandbox_ip_text),
            runtime_kind: &spawned.runtime_kind,
            isolation_mode: &spawned.isolation_mode,
            status: "running",
            pid: Some(spawned.pid),
            cgroup_path: spawned.cgroup_path.as_deref(),
        },
    )?;
    state::emit_event(
        &conn,
        Some(service),
        "service.healthy",
        json!({
            "pid": spawned.pid,
            "sandbox_id": sandbox_id,
            "ip_address": sandbox_ip_text.clone(),
            "runtime_kind": spawned.runtime_kind,
            "isolation_mode": spawned.isolation_mode,
            "cgroup_path": spawned.cgroup_path,
            "health": serde_json::from_str::<Value>(&health_status).unwrap_or_else(|_| json!({"raw": health_status}))
        }),
    )?;
    Ok(())
}

fn cmd_stop(paths: &StatePaths, service: &str) -> Result<()> {
    let value = controlplane::dispatch_ok(
        &paths.home,
        "stop",
        &[("service", Value::String(service.to_string()))],
    )?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

pub fn stop_only(paths: &StatePaths, service: &str) -> Result<()> {
    let record = load_service(paths, service)?;
    if let Some(pid) = record.pid {
        runtime::stop_pid(pid, Duration::from_secs(5))?;
    }
    let conn = state::open(paths)?;
    state::update_service_status(&conn, service, "stopped", None)?;
    if let Some(existing) = state::sandbox_by_service(&conn, service)? {
        state::upsert_sandbox(
            &conn,
            state::SandboxUpsert {
                service_name: service,
                sandbox_id: &existing.sandbox_id,
                hostname: &existing.hostname,
                ip_address: existing.ip_address.as_deref(),
                runtime_kind: &existing.runtime_kind,
                isolation_mode: &existing.isolation_mode,
                status: "stopped",
                pid: None,
                cgroup_path: existing.cgroup_path.as_deref(),
            },
        )?;
    }
    state::emit_event(
        &conn,
        Some(service),
        "service.stopped",
        json!({"pid": record.pid}),
    )?;
    Ok(())
}

fn cmd_destroy(paths: &StatePaths, service: &str) -> Result<()> {
    let value = controlplane::dispatch_ok(
        &paths.home,
        "destroy",
        &[("service", Value::String(service.to_string()))],
    )?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

pub fn destroy_only(paths: &StatePaths, service: &str) -> Result<()> {
    let record = load_service(paths, service)?;
    if let Some(pid) = record.pid {
        let _ = runtime::stop_pid(pid, Duration::from_secs(2));
    }
    let conn = state::open(paths)?;
    if let Some(sandbox) = state::sandbox_by_service(&conn, service)? {
        crate::sandbox::cleanup_sandbox(&crate::sandbox::SandboxLaunchMetadata {
            runtime_kind: sandbox.runtime_kind,
            isolation_mode: sandbox.isolation_mode,
            cgroup_path: sandbox.cgroup_path,
        })?;
    }
    state::emit_event(
        &conn,
        Some(service),
        "service.destroyed",
        json!({"service": service}),
    )?;
    state::delete_sandbox(&conn, service)?;
    state::delete_service(&conn, service)?;
    let runtime_dir = paths.service_runtime_dir(service);
    let logs_dir = paths.service_logs_dir(service);
    let _ = fs::remove_dir_all(runtime_dir);
    let _ = fs::remove_dir_all(logs_dir);
    drop(record);
    Ok(())
}

fn cmd_services(paths: &StatePaths) -> Result<()> {
    let value = controlplane::dispatch_ok(&paths.home, "services", &[])?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn cmd_logs(paths: &StatePaths, service: &str, lines: usize) -> Result<()> {
    let value = controlplane::dispatch_ok(
        &paths.home,
        "logs",
        &[
            ("service", Value::String(service.to_string())),
            ("lines", json!(lines)),
        ],
    )?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

pub fn logs_value(paths: &StatePaths, service: &str, lines: usize) -> Result<serde_json::Value> {
    let stdout_log = paths.service_logs_dir(service).join("stdout.log");
    let stderr_log = paths.service_logs_dir(service).join("stderr.log");
    let stdout = runtime::tail_log(&stdout_log, lines)?;
    let stderr = runtime::tail_log(&stderr_log, lines)?;
    Ok(json!({
        "service": service,
        "stdout": stdout,
        "stderr": stderr
    }))
}

fn cmd_route(paths: &StatePaths, command: RouteCommands) -> Result<()> {
    match command {
        RouteCommands::Add {
            service,
            domain,
            port,
        } => cmd_expose(paths, &service, &domain, port),
        RouteCommands::Sign {
            service,
            domain,
            path,
            expires_in,
            scheme,
        } => {
            let value = controlplane::dispatch_ok(
                &paths.home,
                "route_sign",
                &[
                    ("service", Value::String(service)),
                    ("domain", Value::String(domain)),
                    ("path", Value::String(path)),
                    ("expires_in", json!(expires_in)),
                    ("scheme", Value::String(scheme)),
                ],
            )?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        RouteCommands::List { service } => {
            let value = controlplane::dispatch_ok(
                &paths.home,
                "route_list",
                &[("service", service.map(Value::String).unwrap_or(Value::Null))],
            )?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
    }
}

fn cmd_expose(paths: &StatePaths, service: &str, domain: &str, port: Option<u16>) -> Result<()> {
    let value = controlplane::dispatch_ok(
        &paths.home,
        "route_add",
        &[
            ("service", Value::String(service.to_string())),
            ("domain", Value::String(domain.to_string())),
            ("port", port.map(|v| json!(v)).unwrap_or(Value::Null)),
        ],
    )?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

pub fn expose_value(
    paths: &StatePaths,
    service: &str,
    domain: &str,
    port: Option<u16>,
) -> Result<serde_json::Value> {
    let conn = state::open(paths)?;
    let record = state::service_by_name(&conn, service)?
        .ok_or_else(|| anyhow!("unknown service `{service}`"))?;
    let actual_port = port.or(record.port);
    state::put_route(&conn, service, domain, actual_port)?;
    state::emit_event(
        &conn,
        Some(service),
        "route.created",
        json!({"domain": domain, "port": actual_port}),
    )?;
    Ok(json!({"status": "ok", "service": service, "domain": domain, "port": actual_port}))
}

pub fn signed_link_value(
    paths: &StatePaths,
    service: &str,
    domain: &str,
    target: &str,
    expires_in: u64,
    scheme: &str,
) -> Result<serde_json::Value> {
    let conn = state::open(paths)?;
    let route_exists = state::list_routes(&conn, Some(service))?
        .into_iter()
        .any(|route| route.domain == domain);
    if !route_exists {
        bail!("service `{service}` is not exposed on domain `{domain}`");
    }
    let secret = state::secret_value(&conn, service, "MEGASERVER_SIGNING_KEY")?
        .ok_or_else(|| anyhow!("missing secret `MEGASERVER_SIGNING_KEY` for `{service}`"))?;
    let value = ingress::signed_link(scheme, domain, service, target, expires_in, &secret)?;
    state::emit_event(
        &conn,
        Some(service),
        "route.signed_link.created",
        json!({"domain": domain, "target": target, "expires_in": expires_in, "scheme": scheme}),
    )?;
    Ok(value)
}

fn cmd_volumes(paths: &StatePaths, command: VolumeCommands) -> Result<()> {
    match command {
        VolumeCommands::Create { name, service } => {
            let volume = controlplane::dispatch_ok(
                &paths.home,
                "volume_create",
                &[
                    ("name", Value::String(name)),
                    ("service", service.map(Value::String).unwrap_or(Value::Null)),
                ],
            )?;
            println!("{}", serde_json::to_string_pretty(&volume)?);
        }
        VolumeCommands::List => {
            let volumes = controlplane::dispatch_ok(&paths.home, "volume_list", &[])?;
            println!("{}", serde_json::to_string_pretty(&volumes)?);
        }
    }
    Ok(())
}

fn cmd_secrets(paths: &StatePaths, command: SecretCommands) -> Result<()> {
    match command {
        SecretCommands::Set {
            service,
            key,
            value,
        } => {
            let response = controlplane::dispatch_ok(
                &paths.home,
                "secret_set",
                &[
                    ("service", Value::String(service)),
                    ("key", Value::String(key)),
                    ("value", Value::String(value)),
                ],
            )?;
            println!("{}", serde_json::to_string_pretty(&response)?);
        }
        SecretCommands::List { service } => {
            let secrets = controlplane::dispatch_ok(
                &paths.home,
                "secret_list",
                &[("service", service.map(Value::String).unwrap_or(Value::Null))],
            )?;
            println!("{}", serde_json::to_string_pretty(&secrets)?);
        }
    }
    Ok(())
}

fn cmd_snapshot(paths: &StatePaths, service: &str) -> Result<()> {
    let value = controlplane::dispatch_ok(
        &paths.home,
        "snapshot",
        &[("service", Value::String(service.to_string()))],
    )?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

pub fn snapshot_value(paths: &StatePaths, service: &str) -> Result<serde_json::Value> {
    let record = load_service(paths, service)?;
    let manifest = manifest_from_record(&record)?;
    let snapshot_name = format!("{service}-{}", state::now_string());
    let snapshot_dir = paths.snapshots_dir.join(&snapshot_name);
    fs::create_dir_all(&snapshot_dir)?;
    fs::write(
        snapshot_dir.join("service.json"),
        serde_json::to_vec_pretty(&record)?,
    )?;
    fs::write(
        snapshot_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    runtime::copy_tree(
        &paths.service_runtime_dir(service),
        &snapshot_dir.join("runtime"),
    )?;
    for volume in &manifest.volumes {
        runtime::copy_tree(
            &paths.volumes_dir.join(volume),
            &snapshot_dir.join("volumes").join(volume),
        )?;
    }

    let conn = state::open(paths)?;
    state::insert_snapshot(&conn, service, &snapshot_name, &snapshot_dir)?;
    state::emit_event(
        &conn,
        Some(service),
        "snapshot.created",
        json!({"snapshot": snapshot_name, "path": snapshot_dir}),
    )?;
    Ok(json!({"status": "ok", "snapshot": snapshot_name}))
}

fn cmd_rollback(paths: &StatePaths, service: &str, snapshot: &str) -> Result<()> {
    let value = controlplane::dispatch_ok(
        &paths.home,
        "rollback",
        &[
            ("service", Value::String(service.to_string())),
            ("snapshot", Value::String(snapshot.to_string())),
        ],
    )?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

pub fn rollback_only(paths: &StatePaths, service: &str, snapshot: &str) -> Result<()> {
    let conn = state::open(paths)?;
    let snap = state::snapshot_by_name(&conn, snapshot)?
        .ok_or_else(|| anyhow!("unknown snapshot `{snapshot}`"))?;
    if snap.service_name != service {
        bail!(
            "snapshot `{snapshot}` belongs to `{}`, not `{service}`",
            snap.service_name
        );
    }
    let snapshot_dir = PathBuf::from(&snap.snapshot_path);
    let manifest_raw = fs::read_to_string(snapshot_dir.join("manifest.json"))?;
    let manifest: ServiceManifest = serde_json::from_str(&manifest_raw)?;
    let record = load_service(paths, service)?;
    if let Some(pid) = record.pid {
        let _ = runtime::stop_pid(pid, Duration::from_secs(5));
    }

    for volume in &manifest.volumes {
        let target = paths.volumes_dir.join(volume);
        let _ = fs::remove_dir_all(&target);
        runtime::copy_tree(&snapshot_dir.join("volumes").join(volume), &target)?;
    }
    state::upsert_service(
        &conn,
        service,
        "rolled-back",
        Path::new(&record.app_path),
        &manifest_raw,
        &record.plan_json,
        manifest.network.port,
        manifest.health.path.as_deref(),
    )?;
    state::emit_event(
        &conn,
        Some(service),
        "snapshot.rollback",
        json!({"snapshot": snapshot}),
    )?;
    start_only(paths, service)?;
    Ok(())
}

fn cmd_inspect(paths: &StatePaths, service: &str) -> Result<()> {
    let value = controlplane::dispatch_ok(
        &paths.home,
        "inspect",
        &[("service", Value::String(service.to_string()))],
    )?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

pub fn inspect_value(paths: &StatePaths, service: &str) -> Result<serde_json::Value> {
    let conn = state::open(paths)?;
    let service_record = state::service_by_name(&conn, service)?
        .ok_or_else(|| anyhow!("unknown service `{service}`"))?;
    let routes = state::list_routes(&conn, Some(service))?;
    let secrets = state::list_secrets(&conn, Some(service))?;
    let sandbox = state::sandbox_by_service(&conn, service)?;
    let volumes = state::list_volumes(&conn)?
        .into_iter()
        .filter(|volume| volume.service_name.as_deref() == Some(service))
        .collect::<Vec<_>>();
    let health = if service_record.status == "healthy" || service_record.status == "degraded" {
        runtime::health_check(service_record.port, service_record.health_path.as_deref())
            .unwrap_or_else(|err| {
                json!({"status": "error", "message": err.to_string()}).to_string()
            })
    } else {
        json!({"status": "not-running"}).to_string()
    };

    Ok(json!({
        "service": service_record,
        "routes": routes,
        "volumes": volumes,
        "sandbox": sandbox,
        "secrets": secrets.into_iter().map(|secret| json!({"key": secret.key, "value": "***"})).collect::<Vec<_>>(),
        "health": serde_json::from_str::<Value>(&health).unwrap_or_else(|_| json!({"raw": health})),
    }))
}

fn cmd_events(paths: &StatePaths, service: Option<&str>) -> Result<()> {
    let value = controlplane::dispatch_ok(
        &paths.home,
        "events",
        &[(
            "service",
            service
                .map(|s| Value::String(s.to_string()))
                .unwrap_or(Value::Null),
        )],
    )?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn cmd_shell(paths: &StatePaths, service: &str, command: &[String]) -> Result<()> {
    let value = controlplane::dispatch_ok(
        &paths.home,
        "shell",
        &[
            ("service", Value::String(service.to_string())),
            (
                "command",
                Value::Array(command.iter().cloned().map(Value::String).collect()),
            ),
        ],
    )?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

pub fn shell_only(paths: &StatePaths, service: &str, command: &[String]) -> Result<()> {
    let record = load_service(paths, service)?;
    let manifest = manifest_from_record(&record)?;
    let secret_env = service_secret_env(paths, service)?;
    let volume_env = service_volume_env(paths, service, &manifest)?;
    let conn = state::open(paths)?;
    let sandbox = state::sandbox_by_service(&conn, service)?;
    let sandbox_id_value = sandbox
        .as_ref()
        .map(|sandbox| sandbox.sandbox_id.clone())
        .unwrap_or_else(|| pending_sandbox_id(service));
    let sandbox_hostname_value = sandbox
        .as_ref()
        .map(|sandbox| sandbox.hostname.clone())
        .unwrap_or_else(|| sandbox_hostname(service));
    let sandbox_env =
        sandbox_runtime_env(&conn, service, &sandbox_id_value, &sandbox_hostname_value)?;

    let program_and_args = if command.is_empty() {
        vec!["/bin/sh".to_string()]
    } else {
        command.to_vec()
    };

    let mut cmd = Command::new(&program_and_args[0]);
    if program_and_args.len() > 1 {
        cmd.args(&program_and_args[1..]);
    }
    cmd.current_dir(&record.app_path);
    cmd.env("MEGASERVER_SERVICE", service);
    if let Some(port) = manifest.network.port {
        cmd.env("PORT", port.to_string());
    }
    for (key, value) in secret_env {
        cmd.env(key, value);
    }
    for (key, value) in volume_env {
        cmd.env(key, value);
    }
    for (key, value) in sandbox_env {
        cmd.env(key, value);
    }
    let status = cmd
        .status()
        .with_context(|| format!("launch shell for `{service}`"))?;
    if !status.success() {
        bail!("shell command exited with {status}");
    }
    Ok(())
}

pub fn create_volume_value(
    paths: &StatePaths,
    name: &str,
    service: Option<&str>,
) -> Result<crate::model::VolumeRecord> {
    let conn = state::open(paths)?;
    let volume = state::ensure_volume(&conn, paths, service, name)?;
    state::emit_event(
        &conn,
        service,
        "volume.created",
        json!({"name": name, "host_path": volume.host_path}),
    )?;
    Ok(volume)
}

pub fn set_secret_only(paths: &StatePaths, service: &str, key: &str, value: &str) -> Result<()> {
    let conn = state::open(paths)?;
    state::set_secret(&conn, service, key, value)?;
    state::emit_event(&conn, Some(service), "secret.set", json!({"key": key}))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controlplane;
    use crate::state;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn init_command_sets_up_home() {
        let temp = TempDir::new().unwrap();
        let paths = StatePaths::resolve(Some(temp.path().join("home"))).unwrap();
        state::init(&paths).unwrap();
        assert!(paths.db_path.exists());
        assert!(paths.logs_dir.exists());
    }

    #[test]
    fn control_plane_deploy_reenters_fzy_without_deadlocking() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let temp = TempDir::new().unwrap();
        let paths = StatePaths::resolve(Some(temp.path().join("home"))).unwrap();
        state::init(&paths).unwrap();

        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let example = repo_root.join("examples/hello-service");

        let deployed = controlplane::dispatch_ok(
            &paths.home,
            "deploy",
            &[("app_path", Value::String(example.display().to_string()))],
        )
        .unwrap();

        let service = deployed.get("service").and_then(Value::as_object).unwrap();
        assert_eq!(
            service.get("name").and_then(Value::as_str),
            Some("hello-service")
        );
        assert_eq!(
            service.get("status").and_then(Value::as_str),
            Some("healthy")
        );

        let plan = service.get("plan_json").and_then(Value::as_str).unwrap();
        assert!(plan.contains("\"planner_status\": \"ok\""));
        assert!(plan.contains("\"port\": 18080"));
        assert!(plan.contains("\"command\": ["));

        destroy_only(&paths, "hello-service").unwrap();
    }

    #[test]
    fn http_control_plane_preserves_home_and_json_body() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let temp = TempDir::new().unwrap();
        let paths = StatePaths::resolve(Some(temp.path().join("api-home"))).unwrap();
        state::init(&paths).unwrap();

        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let example = repo_root.join("examples/hello-service");
        let body = json!({"app_path": example.display().to_string()});

        let deployed =
            controlplane::dispatch_http_ok(&paths.home, "POST", "/v1/services/deploy", Some(&body))
                .unwrap();

        let volumes = deployed
            .get("volumes")
            .and_then(Value::as_array)
            .expect("deploy volumes array");
        let host_path = volumes[0]
            .get("host_path")
            .and_then(Value::as_str)
            .expect("volume host path");
        assert!(host_path.starts_with(paths.home.to_string_lossy().as_ref()));

        let listed = controlplane::dispatch_http_ok(
            &paths.home,
            "GET",
            "/v1/routes?service=hello-service",
            None,
        )
        .unwrap();
        assert_eq!(listed.as_array().map(Vec::len), Some(1));

        shell_only(
            &paths,
            "hello-service",
            &[
                "python3".to_string(),
                "-c".to_string(),
                "import os; assert os.environ['MEGASERVER_SANDBOX_IP'] == '10.42.0.1'; assert os.environ['MEGASERVER_SANDBOX_HOSTNAME'] == 'hello-service.sandbox.megaserver'".to_string(),
            ],
        )
        .unwrap();

        destroy_only(&paths, "hello-service").unwrap();
    }
}

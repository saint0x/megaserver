use crate::manifest::ServiceManifest;
use crate::sandbox;
use crate::state::{StatePaths, now_string};
use anyhow::{Context, Result, bail};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub struct SpawnedService {
    pub pid: i32,
    pub stdout_log: PathBuf,
    pub stderr_log: PathBuf,
    pub runtime_kind: String,
    pub isolation_mode: String,
    pub cgroup_path: Option<String>,
}

pub fn spawn_service(
    paths: &StatePaths,
    service_name: &str,
    app_path: &Path,
    manifest: &ServiceManifest,
    secret_env: &[(String, String)],
    volume_env: &[(String, String)],
    sandbox_env: &[(String, String)],
) -> Result<SpawnedService> {
    let logs_dir = paths.service_logs_dir(service_name);
    let runtime_dir = paths.service_runtime_dir(service_name);
    fs::create_dir_all(&logs_dir)?;
    fs::create_dir_all(&runtime_dir)?;

    let stdout_log = logs_dir.join("stdout.log");
    let stderr_log = logs_dir.join("stderr.log");
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stdout_log)?;
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stderr_log)?;

    let mut command = Command::new(&manifest.runtime.command[0]);
    if manifest.runtime.command.len() > 1 {
        command.args(&manifest.runtime.command[1..]);
    }
    command.current_dir(app_path);
    command.stdin(Stdio::null());
    command.stdout(Stdio::from(stdout));
    command.stderr(Stdio::from(stderr));
    command.env("MEGASERVER_SERVICE", service_name);
    command.env("MEGASERVER_STARTED_AT", now_string());
    if let Some(port) = manifest.network.port {
        command.env("PORT", port.to_string());
        command.env("MEGASERVER_PORT", port.to_string());
    }
    for (key, value) in secret_env {
        command.env(key, value);
    }
    for (key, value) in volume_env {
        command.env(key, value);
    }
    for (key, value) in sandbox_env {
        command.env(key, value);
    }
    let sandbox = sandbox::configure_command(
        &mut command,
        service_name,
        &runtime_dir,
        manifest,
        sandbox_env,
    )?;
    let child = command.spawn().with_context(|| {
        format!(
            "failed to spawn `{}` for service `{service_name}`",
            manifest.runtime.command.join(" ")
        )
    })?;

    Ok(SpawnedService {
        pid: i32::try_from(child.id()).unwrap_or(i32::MAX),
        stdout_log,
        stderr_log,
        runtime_kind: sandbox.runtime_kind,
        isolation_mode: sandbox.isolation_mode,
        cgroup_path: sandbox.cgroup_path,
    })
}

pub fn stop_pid(pid: i32, timeout: Duration) -> Result<()> {
    if !process_alive(pid) {
        return Ok(());
    }
    kill(Pid::from_raw(pid), Signal::SIGTERM).context("failed to send SIGTERM")?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_alive(pid) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(150));
    }
    if process_alive(pid) {
        kill(Pid::from_raw(pid), Signal::SIGKILL).context("failed to send SIGKILL")?;
    }
    Ok(())
}

pub fn process_alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

pub fn health_check(host: &str, port: Option<u16>, health_path: Option<&str>) -> Result<String> {
    let Some(port) = port else {
        return Ok("no-port-configured".to_string());
    };
    let path = health_path.unwrap_or("/");
    let url = format!("http://{host}:{port}{path}");
    let start = Instant::now();
    let deadline = start + Duration::from_secs(10);
    let agent = ureq::AgentBuilder::new()
        .timeout_read(Duration::from_secs(2))
        .timeout_connect(Duration::from_secs(2))
        .build();

    while Instant::now() < deadline {
        match agent.get(&url).call() {
            Ok(response) if response.status() < 500 => {
                return Ok(json!({
                    "status": "healthy",
                    "url": url,
                    "code": response.status()
                })
                .to_string());
            }
            Ok(response) => {
                thread::sleep(Duration::from_millis(250));
                if response.status() < 500 {
                    return Ok(
                        json!({"status": "healthy", "url": url, "code": response.status()})
                            .to_string(),
                    );
                }
            }
            Err(_) => thread::sleep(Duration::from_millis(250)),
        }
    }

    bail!("health check failed for {url}")
}

pub fn tail_log(path: &Path, lines: usize) -> Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    let mut file = OpenOptions::new().read(true).open(path)?;
    let len = file.metadata()?.len();
    let seek_back = len.min(64 * 1024);
    file.seek(SeekFrom::Start(len.saturating_sub(seek_back)))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    let collected = buf
        .lines()
        .rev()
        .take(lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    Ok(if collected.is_empty() {
        collected
    } else {
        format!("{collected}\n")
    })
}

pub fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            fs::copy(&from, &to)
                .with_context(|| format!("copy {} -> {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

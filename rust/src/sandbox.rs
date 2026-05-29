use crate::manifest::ServiceManifest;
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct SandboxLaunchMetadata {
    pub service_name: Option<String>,
    pub runtime_kind: String,
    pub isolation_mode: String,
    pub cgroup_path: Option<String>,
}

#[cfg(target_os = "linux")]
const SANDBOX_ROOT_DIR: &str = "rootfs";
#[cfg(target_os = "linux")]
const SANDBOX_APP_RUNTIME_PATH: &str = "/srv/app";

impl SandboxLaunchMetadata {
    fn host_process() -> Self {
        Self {
            service_name: None,
            runtime_kind: "host-process".to_string(),
            isolation_mode: "process-supervision".to_string(),
            cgroup_path: None,
        }
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone)]
struct SandboxResourceConfig {
    memory_limit_bytes: Option<u64>,
    cpu_quota_micros: Option<i64>,
    cpu_period_micros: u64,
    cpu_weight: Option<u64>,
    pids_limit: Option<u64>,
}

pub fn build_service_command(
    service_name: &str,
    app_path: &Path,
    runtime_dir: &Path,
    manifest: &ServiceManifest,
    workspace_app: &Path,
    secret_env: &[(String, String)],
    volume_env: &[(String, String)],
    volume_mounts: &[(std::path::PathBuf, std::path::PathBuf)],
    sandbox_env: &[(String, String)],
) -> Result<(Command, SandboxLaunchMetadata)> {
    #[cfg(target_os = "linux")]
    {
        if crate::network::linux::isolation_supported() {
            return linux::build_service_command(
                service_name,
                app_path,
                runtime_dir,
                manifest,
                workspace_app,
                secret_env,
                volume_env,
                volume_mounts,
                sandbox_env,
            );
        }
        return Ok((
            host_process_command(
                service_name,
                app_path,
                manifest,
                secret_env,
                volume_env,
                sandbox_env,
            ),
            SandboxLaunchMetadata::host_process(),
        ));
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (
            service_name,
            app_path,
            runtime_dir,
            manifest,
            workspace_app,
            secret_env,
            volume_env,
            volume_mounts,
            sandbox_env,
        );
        Ok((
            host_process_command(
                service_name,
                app_path,
                manifest,
                secret_env,
                volume_env,
                sandbox_env,
            ),
            SandboxLaunchMetadata::host_process(),
        ))
    }
}

fn host_process_command(
    service_name: &str,
    app_path: &Path,
    manifest: &ServiceManifest,
    secret_env: &[(String, String)],
    volume_env: &[(String, String)],
    sandbox_env: &[(String, String)],
) -> Command {
    let mut command = Command::new(&manifest.runtime.command[0]);
    if manifest.runtime.command.len() > 1 {
        command.args(&manifest.runtime.command[1..]);
    }
    command.current_dir(app_path);
    command.env("MEGASERVER_SERVICE", service_name);
    command.env("MEGASERVER_STARTED_AT", crate::state::now_string());
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
    command
}

pub fn cleanup_sandbox(metadata: &SandboxLaunchMetadata) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        return linux::cleanup_sandbox(metadata);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = metadata;
        Ok(())
    }
}

pub fn configure_shell_command(
    command: &mut Command,
    runtime_kind: &str,
    sandbox_pid: Option<i32>,
    sandbox_root: Option<&Path>,
    current_dir: Option<&Path>,
) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if runtime_kind == "linux-namespace"
            && crate::network::linux::isolation_supported()
            && let Some(pid) = sandbox_pid
        {
            return linux::configure_command_for_existing_sandbox(
                command,
                pid,
                sandbox_root.map(Path::to_path_buf),
                current_dir.map(Path::to_path_buf),
            );
        }
    }

    let _ = (
        command,
        runtime_kind,
        sandbox_pid,
        sandbox_root,
        current_dir,
    );
    Ok(())
}

pub fn run_sandbox_init(args: crate::cli::SandboxInitArgs) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        return linux::run_sandbox_init(args);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = args;
        anyhow::bail!("sandbox init is only supported on linux");
    }
}

pub fn sandbox_app_runtime_path() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        return SANDBOX_APP_RUNTIME_PATH;
    }

    #[cfg(not(target_os = "linux"))]
    {
        "/"
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_memory_limit(raw: Option<&str>) -> Result<Option<u64>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let lower = raw.to_ascii_lowercase();
    let (digits, unit) = lower
        .chars()
        .partition::<String, _>(|ch| ch.is_ascii_digit() || *ch == '.');
    let value: f64 = digits
        .parse()
        .with_context(|| format!("invalid memory limit `{raw}`"))?;
    let multiplier = match unit.trim() {
        "" | "b" => 1_f64,
        "k" | "kb" => 1024_f64,
        "m" | "mb" => 1024_f64 * 1024_f64,
        "g" | "gb" => 1024_f64 * 1024_f64 * 1024_f64,
        "t" | "tb" => 1024_f64 * 1024_f64 * 1024_f64 * 1024_f64,
        other => anyhow::bail!("unsupported memory unit `{other}` in `{raw}`"),
    };
    Ok(Some((value * multiplier) as u64))
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_cpu_limit(raw: Option<&str>) -> Result<(Option<i64>, Option<u64>)> {
    let Some(raw) = raw else {
        return Ok((None, Some(100)));
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok((None, Some(100)));
    }
    let period = 100_000_u64;
    let milli_cores = if let Some(prefix) = raw.strip_suffix('m') {
        prefix
            .trim()
            .parse::<u64>()
            .with_context(|| format!("invalid cpu limit `{raw}`"))?
    } else {
        let cpus: f64 = raw
            .parse()
            .with_context(|| format!("invalid cpu limit `{raw}`"))?;
        (cpus * 1000.0) as u64
    };
    let quota = ((milli_cores as u128) * (period as u128) / 1000_u128) as i64;
    let weight = ((milli_cores.max(100) * 100) / 1000).clamp(1, 10_000);
    Ok((Some(quota.max(1)), Some(weight)))
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn resource_config(manifest: &ServiceManifest) -> Result<SandboxResourceConfig> {
    let memory_limit_bytes = parse_memory_limit(manifest.resources.memory.as_deref())?;
    let (cpu_quota_micros, cpu_weight) = parse_cpu_limit(manifest.resources.cpu.as_deref())?;
    Ok(SandboxResourceConfig {
        memory_limit_bytes,
        cpu_quota_micros,
        cpu_period_micros: 100_000,
        cpu_weight,
        pids_limit: Some(512),
    })
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{SandboxLaunchMetadata, resource_config};
    use crate::manifest::ServiceManifest;
    use crate::network;
    use anyhow::{Context, Result};
    use nix::mount::{MsFlags, mount};
    use nix::sched::{CloneFlags, setns};
    use nix::sys::signal::{SigSet, Signal, kill};
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    use nix::unistd::{Pid, chdir, chroot, sethostname};
    use std::fs;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::{fs::File, os::fd::AsFd};

    pub fn build_service_command(
        service_name: &str,
        app_path: &Path,
        runtime_dir: &Path,
        manifest: &ServiceManifest,
        workspace_app: &Path,
        secret_env: &[(String, String)],
        volume_env: &[(String, String)],
        volume_mounts: &[(PathBuf, PathBuf)],
        sandbox_env: &[(String, String)],
    ) -> Result<(Command, SandboxLaunchMetadata)> {
        let hostname = sandbox_env
            .iter()
            .find(|(key, _)| key == "MEGASERVER_SANDBOX_HOSTNAME")
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| format!("{service_name}.sandbox.megaserver"));
        let sandbox_ip = sandbox_env
            .iter()
            .find(|(key, _)| key == "MEGASERVER_SANDBOX_IP")
            .map(|(_, value)| value.clone())
            .context("missing MEGASERVER_SANDBOX_IP")?;
        let cgroup_path =
            prepare_cgroup(service_name, manifest).context("prepare sandbox cgroup")?;
        let runtime_mount = runtime_dir.join("sandbox");
        fs::create_dir_all(&runtime_mount)
            .with_context(|| format!("create sandbox runtime dir {}", runtime_mount.display()))?;
        let hosts_path = runtime_mount.join("hosts");
        let resolv_path = runtime_mount.join("resolv.conf");
        let sandbox_root = runtime_mount.join(super::SANDBOX_ROOT_DIR);
        prepare_sandbox_root(&sandbox_root)?;
        fs::write(
            &resolv_path,
            format!(
                "nameserver {}\noptions ndots:0 timeout:1 attempts:2\n",
                crate::network::sandbox_gateway()
            ),
        )
        .with_context(|| format!("write {}", resolv_path.display()))?;
        let netns_name = network::linux::netns_name(service_name);
        network::linux::setup_sandbox_network(service_name, &sandbox_ip)
            .context("prepare sandbox network")?;
        let current_exe = std::env::current_exe().context("resolve megaserver executable")?;
        let mut command = Command::new("ip");
        command.args(["netns", "exec", &netns_name, "unshare"]);
        command.args([
            "--fork",
            "--pid",
            "--mount-proc",
            "--uts",
            "--mount",
            "--ipc",
        ]);
        command.arg(current_exe);
        command.arg("--home");
        command.arg(std::env::var("MEGASERVER_HOME").unwrap_or_else(|_| {
            std::env::current_dir()
                .unwrap()
                .join(".megaserver")
                .display()
                .to_string()
        }));
        command.arg("sandbox-init");
        command.arg("--hostname");
        command.arg(&hostname);
        command.arg("--sandbox-root");
        command.arg(&sandbox_root);
        command.arg("--workspace-app");
        command.arg(workspace_app);
        command.arg("--hosts-path");
        command.arg(&hosts_path);
        command.arg("--resolv-path");
        command.arg(&resolv_path);
        command.arg("--app-runtime-path");
        command.arg(super::SANDBOX_APP_RUNTIME_PATH);
        if let Some(cgroup_path) = &cgroup_path {
            command.arg("--cgroup-path");
            command.arg(cgroup_path);
        }
        for (host_path, target_path) in volume_mounts {
            command.arg("--volume-mount");
            command.arg(format!(
                "{}::{}",
                host_path.display(),
                target_path.display()
            ));
        }
        command.arg("--");
        command.arg(&manifest.runtime.command[0]);
        if manifest.runtime.command.len() > 1 {
            command.args(&manifest.runtime.command[1..]);
        }
        command.current_dir(app_path);
        command.env("MEGASERVER_SERVICE", service_name);
        command.env("MEGASERVER_STARTED_AT", crate::state::now_string());
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

        Ok((
            command,
            SandboxLaunchMetadata {
                service_name: Some(service_name.to_string()),
                runtime_kind: "linux-namespace".to_string(),
                isolation_mode: if cgroup_path.is_some() {
                    "net+pid+uts+mount+ipc+cgroup".to_string()
                } else {
                    "net+pid+uts+mount+ipc".to_string()
                },
                cgroup_path: cgroup_path.map(|path| path.display().to_string()),
            },
        ))
    }

    pub fn cleanup_sandbox(metadata: &SandboxLaunchMetadata) -> Result<()> {
        if metadata.runtime_kind != "linux-namespace" {
            return Ok(());
        }
        let Some(path) = metadata.cgroup_path.as_ref() else {
            network::linux::cleanup_sandbox_network(metadata.service_name.as_deref())?;
            return Ok(());
        };
        let path = PathBuf::from(path);
        if path.exists() {
            fs::remove_dir(&path).with_context(|| format!("remove cgroup {}", path.display()))?;
        }
        network::linux::cleanup_sandbox_network(metadata.service_name.as_deref())?;
        Ok(())
    }

    pub fn configure_command_for_existing_sandbox(
        command: &mut Command,
        sandbox_pid: i32,
        sandbox_root: Option<PathBuf>,
        current_dir: Option<PathBuf>,
    ) -> Result<()> {
        let namespaces = SandboxNamespaces::from_pid(sandbox_pid)?;
        unsafe {
            command.pre_exec(move || {
                join_namespace(&namespaces.net, CloneFlags::CLONE_NEWNET)?;
                join_namespace(&namespaces.uts, CloneFlags::CLONE_NEWUTS)?;
                join_namespace(&namespaces.mount, CloneFlags::CLONE_NEWNS)?;
                join_namespace(&namespaces.ipc, CloneFlags::CLONE_NEWIPC)?;
                if let Some(root) = &sandbox_root {
                    chroot(root).map_err(std::io::Error::other)?;
                }
                if let Some(dir) = &current_dir {
                    chdir(dir).map_err(std::io::Error::other)?;
                }
                Ok(())
            });
        }
        Ok(())
    }

    pub fn run_sandbox_init(args: crate::cli::SandboxInitArgs) -> Result<()> {
        sethostname(&args.hostname).context("set sandbox hostname")?;
        mount(
            None::<&str>,
            "/",
            None::<&str>,
            MsFlags::MS_REC | MsFlags::MS_PRIVATE,
            None::<&str>,
        )
        .context("privatize mount root")?;
        bind_path_into_root(Path::new("/usr"), &args.sandbox_root.join("usr"), true)?;
        bind_path_into_root(Path::new("/bin"), &args.sandbox_root.join("bin"), true)?;
        if Path::new("/sbin").exists() {
            bind_path_into_root(Path::new("/sbin"), &args.sandbox_root.join("sbin"), true)?;
        }
        if Path::new("/lib").exists() {
            bind_path_into_root(Path::new("/lib"), &args.sandbox_root.join("lib"), true)?;
        }
        if Path::new("/lib64").exists() {
            bind_path_into_root(Path::new("/lib64"), &args.sandbox_root.join("lib64"), true)?;
        }
        if Path::new("/etc/ssl").exists() {
            bind_path_into_root(
                Path::new("/etc/ssl"),
                &args.sandbox_root.join("etc").join("ssl"),
                true,
            )?;
        }
        if Path::new("/etc/nsswitch.conf").exists() {
            bind_path_into_root(
                Path::new("/etc/nsswitch.conf"),
                &args.sandbox_root.join("etc").join("nsswitch.conf"),
                true,
            )?;
        }
        for device in [
            "/dev/null",
            "/dev/zero",
            "/dev/random",
            "/dev/urandom",
            "/dev/tty",
        ] {
            let source = Path::new(device);
            if source.exists() {
                let target = remap_into_root(&args.sandbox_root, source)?;
                bind_path_into_root(source, &target, false)?;
            }
        }
        mount(
            Some("proc"),
            &args.sandbox_root.join("proc"),
            Some("proc"),
            MsFlags::empty(),
            None::<&str>,
        )
        .context("mount sandbox proc")?;
        mount(
            Some("tmpfs"),
            &args.sandbox_root.join("tmp"),
            Some("tmpfs"),
            MsFlags::empty(),
            Some("mode=1777,size=64m"),
        )
        .context("mount sandbox tmpfs")?;
        mount(
            Some(args.workspace_app.as_path()),
            &args.sandbox_root.join("srv").join("app"),
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        )
        .context("bind workspace app")?;
        mount(
            Some(args.workspace_app.as_path()),
            &args.sandbox_root.join("srv").join("app"),
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
            None::<&str>,
        )
        .context("remount workspace app readonly")?;
        for mount_spec in &args.volume_mounts {
            let (host, target) = parse_volume_mount_spec(mount_spec)?;
            let target = remap_into_root(&args.sandbox_root, &target)?;
            fs::create_dir_all(&target)
                .with_context(|| format!("create volume target {}", target.display()))?;
            mount(
                Some(host.as_path()),
                target.as_path(),
                None::<&str>,
                MsFlags::MS_BIND | MsFlags::MS_REC,
                None::<&str>,
            )
            .with_context(|| format!("bind volume {} -> {}", host.display(), target.display()))?;
        }
        mount(
            Some(args.hosts_path.as_path()),
            &args.sandbox_root.join("etc").join("hosts"),
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        )
        .context("bind /etc/hosts")?;
        mount(
            Some(args.resolv_path.as_path()),
            &args.sandbox_root.join("etc").join("resolv.conf"),
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        )
        .context("bind /etc/resolv.conf")?;
        if let Some(cgroup_path) = &args.cgroup_path {
            join_cgroup(cgroup_path).context("join sandbox cgroup")?;
        }

        let mut signals = SigSet::empty();
        signals.add(Signal::SIGTERM);
        signals.add(Signal::SIGINT);
        signals.add(Signal::SIGHUP);
        signals.add(Signal::SIGCHLD);
        signals
            .thread_block()
            .context("block sandbox init signals")?;
        chroot(&args.sandbox_root).context("chroot sandbox root")?;
        chdir(args.app_runtime_path.as_path()).context("enter sandbox app directory")?;

        let (program, rest) = args
            .command
            .split_first()
            .context("sandbox init missing program")?;
        let mut child = Command::new(program);
        child.args(rest);
        child.current_dir(&args.app_runtime_path);
        let child = child
            .spawn()
            .with_context(|| format!("spawn sandbox workload `{program}`"))?;
        let child_pid = Pid::from_raw(i32::try_from(child.id()).unwrap_or(i32::MAX));

        loop {
            match signals.wait().context("wait for sandbox signal")? {
                Signal::SIGCHLD => {
                    let mut main_exited = None;
                    while let Ok(status) = waitpid(None, Some(WaitPidFlag::WNOHANG)) {
                        match status {
                            WaitStatus::StillAlive => break,
                            WaitStatus::Exited(pid, code) if pid == child_pid => {
                                main_exited = Some(code);
                            }
                            WaitStatus::Signaled(pid, signal, _) if pid == child_pid => {
                                let code = 128 + signal as i32;
                                main_exited = Some(code);
                            }
                            WaitStatus::Exited(_, _)
                            | WaitStatus::Signaled(_, _, _)
                            | WaitStatus::Stopped(_, _)
                            | WaitStatus::Continued(_) => {}
                            _ => {}
                        }
                    }
                    if let Some(code) = main_exited {
                        std::process::exit(code);
                    }
                }
                Signal::SIGTERM | Signal::SIGINT | Signal::SIGHUP => {
                    let signal = Signal::SIGTERM;
                    let _ = kill(child_pid, signal);
                }
                _ => {}
            }
        }
    }

    fn prepare_cgroup(service_name: &str, manifest: &ServiceManifest) -> Result<Option<PathBuf>> {
        let root = Path::new("/sys/fs/cgroup");
        if !root.exists() {
            anyhow::bail!(
                "linux sandboxing requires cgroup v2 root at {}",
                root.display()
            );
        }
        let sandbox_root = root.join("megaserver");
        fs::create_dir_all(&sandbox_root)
            .with_context(|| format!("create cgroup root {}", sandbox_root.display()))?;
        validate_cgroup_controllers(root, &sandbox_root)?;
        let cgroup_path = sandbox_root.join(service_name);
        fs::create_dir_all(&cgroup_path)
            .with_context(|| format!("create cgroup {}", cgroup_path.display()))?;

        let controllers = sandbox_root.join("cgroup.subtree_control");
        if controllers.exists() {
            fs::write(&controllers, "+memory +cpu +pids")
                .with_context(|| format!("enable controllers in {}", controllers.display()))?;
        }

        let resources = resource_config(manifest)?;
        if let Some(memory_limit_bytes) = resources.memory_limit_bytes {
            let _ = fs::write(
                cgroup_path.join("memory.max"),
                memory_limit_bytes.to_string(),
            );
            let _ = fs::write(cgroup_path.join("memory.swap.max"), "0");
        }
        if let Some(cpu_quota_micros) = resources.cpu_quota_micros {
            let _ = fs::write(
                cgroup_path.join("cpu.max"),
                format!("{cpu_quota_micros} {}", resources.cpu_period_micros),
            );
        }
        if let Some(cpu_weight) = resources.cpu_weight {
            let _ = fs::write(cgroup_path.join("cpu.weight"), cpu_weight.to_string());
        }
        if let Some(pids_limit) = resources.pids_limit {
            fs::write(cgroup_path.join("pids.max"), pids_limit.to_string())
                .with_context(|| format!("write pids.max for {}", cgroup_path.display()))?;
        }

        Ok(Some(cgroup_path))
    }

    fn validate_cgroup_controllers(root: &Path, sandbox_root: &Path) -> Result<()> {
        let available = fs::read_to_string(root.join("cgroup.controllers"))
            .context("read cgroup.controllers")?;
        for controller in ["memory", "cpu", "pids"] {
            if !available
                .split_whitespace()
                .any(|entry| entry == controller)
            {
                anyhow::bail!("linux sandboxing requires cgroup controller `{controller}`");
            }
        }
        let control = sandbox_root.join("cgroup.subtree_control");
        if control.exists() {
            let parent_type = root.join("cgroup.type");
            if parent_type.exists() {
                let kind = fs::read_to_string(parent_type).unwrap_or_default();
                if kind.trim() != "domain" && !kind.trim().is_empty() {
                    anyhow::bail!(
                        "linux sandboxing requires domain cgroup at {}",
                        root.display()
                    );
                }
            }
        }
        Ok(())
    }

    fn join_cgroup(cgroup_path: &Path) -> Result<()> {
        let procs = cgroup_path.join("cgroup.procs");
        fs::write(&procs, "0").with_context(|| format!("join cgroup {}", cgroup_path.display()))?;
        Ok(())
    }

    struct SandboxNamespaces {
        net: PathBuf,
        uts: PathBuf,
        mount: PathBuf,
        ipc: PathBuf,
    }

    impl SandboxNamespaces {
        fn from_pid(pid: i32) -> Result<Self> {
            let root = PathBuf::from(format!("/proc/{pid}/ns"));
            Ok(Self {
                net: root.join("net"),
                uts: root.join("uts"),
                mount: root.join("mnt"),
                ipc: root.join("ipc"),
            })
        }
    }

    fn join_namespace(path: &Path, flag: CloneFlags) -> std::io::Result<()> {
        let namespace = File::open(path)?;
        setns(namespace.as_fd(), flag).map_err(std::io::Error::other)
    }

    fn parse_volume_mount_spec(raw: &str) -> Result<(PathBuf, PathBuf)> {
        let (host, target) = raw.split_once("::").context("invalid volume mount spec")?;
        Ok((PathBuf::from(host), PathBuf::from(target)))
    }

    pub(super) fn prepare_sandbox_root(root: &Path) -> Result<()> {
        let _ = fs::remove_dir_all(root);
        for dir in [
            root.to_path_buf(),
            root.join("etc"),
            root.join("usr"),
            root.join("bin"),
            root.join("srv"),
            root.join("srv").join("app"),
            root.join("dev"),
            root.join("proc"),
            root.join("tmp"),
            root.join("run"),
            root.join("var"),
            root.join("var").join("run"),
        ] {
            fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        }
        if Path::new("/sbin").exists() {
            fs::create_dir_all(root.join("sbin"))?;
        }
        if Path::new("/lib").exists() {
            fs::create_dir_all(root.join("lib"))?;
        }
        if Path::new("/lib64").exists() {
            fs::create_dir_all(root.join("lib64"))?;
        }
        Ok(())
    }

    fn bind_path_into_root(source: &Path, target: &Path, readonly: bool) -> Result<()> {
        if source.is_dir() {
            fs::create_dir_all(target).with_context(|| format!("create {}", target.display()))?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            if !target.exists() {
                fs::File::create(target).with_context(|| format!("create {}", target.display()))?;
            }
        }
        mount(
            Some(source),
            target,
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        )
        .with_context(|| format!("bind {} -> {}", source.display(), target.display()))?;
        if readonly {
            mount(
                Some(source),
                target,
                None::<&str>,
                MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
                None::<&str>,
            )
            .with_context(|| format!("remount readonly {}", target.display()))?;
        }
        Ok(())
    }

    pub(super) fn remap_into_root(root: &Path, target: &Path) -> Result<PathBuf> {
        let relative = target
            .strip_prefix("/")
            .with_context(|| format!("expected absolute target path {}", target.display()))?;
        Ok(root.join(relative))
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_cpu_limit, parse_memory_limit, resource_config};
    use crate::manifest::{ResourceSection, ServiceManifest};
    #[cfg(target_os = "linux")]
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    fn parses_memory_units() {
        assert_eq!(
            parse_memory_limit(Some("128mb")).unwrap(),
            Some(134_217_728)
        );
        assert_eq!(
            parse_memory_limit(Some("1gb")).unwrap(),
            Some(1_073_741_824)
        );
        assert_eq!(parse_memory_limit(None).unwrap(), None);
    }

    #[test]
    fn parses_cpu_units() {
        let (quota, weight) = parse_cpu_limit(Some("500m")).unwrap();
        assert_eq!(quota, Some(50_000));
        assert_eq!(weight, Some(50));

        let (quota, weight) = parse_cpu_limit(Some("2")).unwrap();
        assert_eq!(quota, Some(200_000));
        assert_eq!(weight, Some(200));
    }

    #[test]
    fn builds_resource_config_from_manifest() {
        let manifest = ServiceManifest {
            name: "demo".to_string(),
            runtime: Default::default(),
            network: Default::default(),
            resources: ResourceSection {
                memory: Some("256mb".to_string()),
                cpu: Some("750m".to_string()),
            },
            volumes: vec![],
            routes: vec![],
            health: Default::default(),
        };
        let config = resource_config(&manifest).unwrap();
        assert_eq!(config.memory_limit_bytes, Some(268_435_456));
        assert_eq!(config.cpu_quota_micros, Some(75_000));
        assert_eq!(config.cpu_weight, Some(75));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn prepares_sandbox_root_layout() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("rootfs");
        super::linux::prepare_sandbox_root(&root).unwrap();
        for path in [
            root.join("etc"),
            root.join("srv").join("app"),
            root.join("dev"),
            root.join("proc"),
            root.join("tmp"),
            root.join("run"),
            root.join("var").join("run"),
        ] {
            assert!(path.exists(), "missing {}", path.display());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn remaps_absolute_paths_into_root() {
        let temp = TempDir::new().unwrap();
        let mapped =
            super::linux::remap_into_root(temp.path(), Path::new("/srv/app/data")).unwrap();
        assert_eq!(mapped, temp.path().join("srv/app/data"));
    }
}

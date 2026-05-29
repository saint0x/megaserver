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

pub fn configure_command(
    command: &mut Command,
    service_name: &str,
    runtime_dir: &Path,
    manifest: &ServiceManifest,
    sandbox_env: &[(String, String)],
) -> Result<SandboxLaunchMetadata> {
    #[cfg(target_os = "linux")]
    {
        if crate::network::linux::isolation_supported() {
            return linux::configure_command(
                command,
                service_name,
                runtime_dir,
                manifest,
                sandbox_env,
            );
        }
        let _ = (command, service_name, runtime_dir, manifest, sandbox_env);
        return Ok(SandboxLaunchMetadata::host_process());
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (command, service_name, runtime_dir, manifest, sandbox_env);
        Ok(SandboxLaunchMetadata::host_process())
    }
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
) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if runtime_kind == "linux-namespace"
            && crate::network::linux::isolation_supported()
            && let Some(pid) = sandbox_pid
        {
            return linux::configure_command_for_existing_sandbox(command, pid);
        }
    }

    let _ = (command, runtime_kind, sandbox_pid);
    Ok(())
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
    use nix::sched::{CloneFlags, setns, unshare};
    use nix::unistd::{sethostname, setsid};
    use std::fs;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::{fs::File, os::fd::AsFd};

    pub fn configure_command(
        command: &mut Command,
        service_name: &str,
        runtime_dir: &Path,
        manifest: &ServiceManifest,
        sandbox_env: &[(String, String)],
    ) -> Result<SandboxLaunchMetadata> {
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
        let hosts_mount = hosts_path.exists().then_some(hosts_path.clone());
        let resolv_path = runtime_mount.join("resolv.conf");
        fs::write(
            &resolv_path,
            format!(
                "nameserver {}\noptions ndots:0 timeout:1 attempts:2\n",
                crate::network::sandbox_gateway()
            ),
        )
        .with_context(|| format!("write {}", resolv_path.display()))?;
        let resolv_mount = resolv_path.exists().then_some(resolv_path.clone());
        let netns_name = network::linux::netns_name(service_name);
        network::linux::setup_sandbox_network(service_name, &sandbox_ip)
            .context("prepare sandbox network")?;

        let cgroup_path_for_exec = cgroup_path.clone();
        let hostname_for_exec = hostname.clone();
        let netns_path = PathBuf::from("/var/run/netns").join(&netns_name);

        unsafe {
            command.pre_exec(move || {
                setsid().map_err(std::io::Error::other)?;
                let netns = File::open(&netns_path).map_err(std::io::Error::other)?;
                setns(netns.as_fd(), CloneFlags::CLONE_NEWNET).map_err(std::io::Error::other)?;
                unshare(
                    CloneFlags::CLONE_NEWUTS | CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWIPC,
                )
                .map_err(std::io::Error::other)?;
                sethostname(&hostname_for_exec).map_err(std::io::Error::other)?;
                mount(
                    None::<&str>,
                    "/",
                    None::<&str>,
                    MsFlags::MS_REC | MsFlags::MS_PRIVATE,
                    None::<&str>,
                )
                .map_err(std::io::Error::other)?;
                if let Some(hosts_path) = &hosts_mount {
                    mount(
                        Some(hosts_path.as_path()),
                        "/etc/hosts",
                        None::<&str>,
                        MsFlags::MS_BIND,
                        None::<&str>,
                    )
                    .map_err(std::io::Error::other)?;
                }
                if let Some(resolv_path) = &resolv_mount {
                    mount(
                        Some(resolv_path.as_path()),
                        "/etc/resolv.conf",
                        None::<&str>,
                        MsFlags::MS_BIND,
                        None::<&str>,
                    )
                    .map_err(std::io::Error::other)?;
                }
                if let Some(cgroup_path) = &cgroup_path_for_exec {
                    join_cgroup(cgroup_path).map_err(std::io::Error::other)?;
                }
                Ok(())
            });
        }

        Ok(SandboxLaunchMetadata {
            service_name: Some(service_name.to_string()),
            runtime_kind: "linux-namespace".to_string(),
            isolation_mode: if cgroup_path.is_some() {
                "net+uts+mount+ipc+cgroup".to_string()
            } else {
                "net+uts+mount+ipc".to_string()
            },
            cgroup_path: cgroup_path.map(|path| path.display().to_string()),
        })
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
    ) -> Result<()> {
        let namespaces = SandboxNamespaces::from_pid(sandbox_pid)?;
        unsafe {
            command.pre_exec(move || {
                join_namespace(&namespaces.net, CloneFlags::CLONE_NEWNET)?;
                join_namespace(&namespaces.uts, CloneFlags::CLONE_NEWUTS)?;
                join_namespace(&namespaces.mount, CloneFlags::CLONE_NEWNS)?;
                join_namespace(&namespaces.ipc, CloneFlags::CLONE_NEWIPC)?;
                Ok(())
            });
        }
        Ok(())
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
}

#[cfg(test)]
mod tests {
    use super::{parse_cpu_limit, parse_memory_limit, resource_config};
    use crate::manifest::{ResourceSection, ServiceManifest};

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
}

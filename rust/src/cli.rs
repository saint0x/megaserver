use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "megaserver",
    version,
    about = "Self-hosted single-node PaaS runtime"
)]
pub struct Cli {
    #[arg(long, global = true)]
    pub home: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    Init,
    Deploy(DeployArgs),
    Start(ServiceArg),
    Stop(ServiceArg),
    Restart(ServiceArg),
    Destroy(ServiceArg),
    Ps,
    Services,
    Logs(LogsArgs),
    Route(RouteArgs),
    Expose(ExposeArgs),
    Volumes(VolumesArgs),
    Secrets(SecretsArgs),
    Snapshot(ServiceArg),
    Rollback(RollbackArgs),
    Inspect(ServiceArg),
    Events(EventsArgs),
    Shell(ShellArgs),
    Daemon(DaemonArgs),
    #[command(hide = true)]
    Dns(DnsArgs),
    #[command(hide = true)]
    SandboxInit(SandboxInitArgs),
}

#[derive(Debug, Args)]
pub struct DeployArgs {
    pub app_path: PathBuf,
}

#[derive(Debug, Args)]
pub struct ServiceArg {
    pub service: String,
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    pub service: String,
    #[arg(long, default_value_t = 50)]
    pub lines: usize,
}

#[derive(Debug, Args)]
pub struct RouteArgs {
    #[command(subcommand)]
    pub command: RouteCommands,
}

#[derive(Debug, Subcommand)]
pub enum RouteCommands {
    Add {
        service: String,
        domain: String,
        #[arg(long)]
        port: Option<u16>,
    },
    Sign {
        service: String,
        domain: String,
        path: String,
        #[arg(long, default_value_t = 300)]
        expires_in: u64,
        #[arg(long, default_value = "https")]
        scheme: String,
    },
    List {
        #[arg(long)]
        service: Option<String>,
    },
}

#[derive(Debug, Args)]
pub struct ExposeArgs {
    pub service: String,
    pub domain: String,
    #[arg(long)]
    pub port: Option<u16>,
}

#[derive(Debug, Args)]
pub struct VolumesArgs {
    #[command(subcommand)]
    pub command: VolumeCommands,
}

#[derive(Debug, Subcommand)]
pub enum VolumeCommands {
    Create {
        name: String,
        #[arg(long)]
        service: Option<String>,
    },
    List,
}

#[derive(Debug, Args)]
pub struct SecretsArgs {
    #[command(subcommand)]
    pub command: SecretCommands,
}

#[derive(Debug, Subcommand)]
pub enum SecretCommands {
    Set {
        service: String,
        key: String,
        value: String,
    },
    List {
        #[arg(long)]
        service: Option<String>,
    },
}

#[derive(Debug, Args)]
pub struct RollbackArgs {
    pub service: String,
    pub snapshot: String,
}

#[derive(Debug, Args)]
pub struct EventsArgs {
    #[arg(long)]
    pub service: Option<String>,
}

#[derive(Debug, Args)]
pub struct ShellArgs {
    pub service: String,
    #[arg(last = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct DaemonArgs {
    #[arg(long, default_value = "127.0.0.1:7700")]
    pub bind: String,
    #[arg(long, default_value = "127.0.0.1:8088")]
    pub ingress_bind: String,
    #[arg(long)]
    pub tls_cert: Option<PathBuf>,
    #[arg(long)]
    pub tls_key: Option<PathBuf>,
    #[arg(long)]
    pub tls_ca: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct DnsArgs {
    #[arg(long, default_value = "10.42.0.254:53")]
    pub bind: String,
}

#[derive(Debug, Args)]
pub struct SandboxInitArgs {
    #[arg(long)]
    pub hostname: String,
    #[arg(long)]
    pub sandbox_root: PathBuf,
    #[arg(long)]
    pub workspace_app: PathBuf,
    #[arg(long)]
    pub hosts_path: PathBuf,
    #[arg(long)]
    pub resolv_path: PathBuf,
    #[arg(long)]
    pub cgroup_path: Option<PathBuf>,
    #[arg(long)]
    pub app_runtime_path: PathBuf,
    #[arg(long = "volume-mount")]
    pub volume_mounts: Vec<String>,
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

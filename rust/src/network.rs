use anyhow::{Context, Result, bail};
use ipnet::Ipv4Net;
use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::sync::Arc;
use tokio::sync::RwLock;

#[cfg(target_os = "linux")]
use {
    futures::TryStreamExt,
    rtnetlink::{Handle, new_connection},
    std::io::ErrorKind,
};

pub struct IpamManager {
    state: Arc<RwLock<IpamState>>,
}

struct IpamState {
    subnet: Option<Ipv4Net>,
    allocated: HashSet<Ipv4Addr>,
}

impl IpamManager {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(IpamState {
                subnet: None,
                allocated: HashSet::new(),
            })),
        }
    }

    pub async fn configure_subnet(&self, subnet_str: &str) -> Result<()> {
        let subnet: Ipv4Net = subnet_str.parse().context("invalid subnet CIDR")?;
        if subnet.prefix_len() != 24 {
            bail!("subnet must be /24, got /{}", subnet.prefix_len());
        }
        let mut state = self.state.write().await;
        state.subnet = Some(subnet);
        state.allocated.clear();
        Ok(())
    }

    pub async fn allocate_ip(&self) -> Result<Ipv4Addr> {
        let mut state = self.state.write().await;
        let subnet = state.subnet.context("subnet not configured")?;
        for host in subnet.hosts() {
            if !state.allocated.contains(&host) {
                state.allocated.insert(host);
                return Ok(host);
            }
        }
        bail!("no available IPs in subnet {}", subnet)
    }

    pub async fn release_ip(&self, ip: Ipv4Addr) {
        let mut state = self.state.write().await;
        state.allocated.remove(&ip);
    }
}

pub struct RouteManager {
    state: Arc<RwLock<HashMap<String, String>>>,
    #[cfg(target_os = "linux")]
    handle: Handle,
}

impl RouteManager {
    #[cfg(target_os = "linux")]
    pub async fn new() -> Result<Self> {
        let (connection, handle, _) =
            new_connection().context("failed to create netlink connection")?;
        tokio::spawn(connection);
        Ok(Self {
            state: Arc::new(RwLock::new(HashMap::new())),
            handle,
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn new() -> Result<Self> {
        Ok(Self {
            state: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    #[cfg(target_os = "linux")]
    pub async fn add_route(&self, destination: &str, interface: &str) -> Result<()> {
        let subnet: Ipv4Net = destination.parse().context("invalid destination subnet")?;
        let mut links = self
            .handle
            .link()
            .get()
            .match_name(interface.to_string())
            .execute();
        let link = links
            .try_next()
            .await
            .context("failed to query interface")?
            .context(format!("interface `{interface}` not found"))?;
        let if_index = link.header.index;
        match self
            .handle
            .route()
            .add()
            .v4()
            .destination_prefix(subnet.network(), subnet.prefix_len())
            .output_interface(if_index)
            .execute()
            .await
        {
            Ok(_) => {
                self.state
                    .write()
                    .await
                    .insert(destination.to_string(), interface.to_string());
                Ok(())
            }
            Err(err) => {
                if let Some(io_err) = err.downcast_ref::<std::io::Error>()
                    && io_err.kind() == ErrorKind::AlreadyExists
                {
                    self.state
                        .write()
                        .await
                        .insert(destination.to_string(), interface.to_string());
                    return Ok(());
                }
                bail!("failed to add route: {err}")
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn add_route(&self, destination: &str, interface: &str) -> Result<()> {
        self.state
            .write()
            .await
            .insert(destination.to_string(), interface.to_string());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub async fn remove_route(&self, destination: &str) -> Result<()> {
        let subnet: Ipv4Net = destination.parse().context("invalid destination subnet")?;
        match self
            .handle
            .route()
            .del()
            .v4()
            .destination_prefix(subnet.network(), subnet.prefix_len())
            .execute()
            .await
        {
            Ok(_) => {
                self.state.write().await.remove(destination);
                Ok(())
            }
            Err(err) => {
                if let Some(io_err) = err.downcast_ref::<std::io::Error>()
                    && (io_err.kind() == ErrorKind::NotFound || io_err.raw_os_error() == Some(3))
                {
                    self.state.write().await.remove(destination);
                    return Ok(());
                }
                bail!("failed to remove route: {err}")
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn remove_route(&self, destination: &str) -> Result<()> {
        self.state.write().await.remove(destination);
        Ok(())
    }

    pub async fn routes(&self) -> HashMap<String, String> {
        self.state.read().await.clone()
    }
}

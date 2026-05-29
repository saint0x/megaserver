pub const SANDBOX_SUBNET: &str = "10.42.0.0/24";
pub const BRIDGE_NAME: &str = "megabr0";
pub const BRIDGE_CIDR: &str = "10.42.0.254/24";
pub const BRIDGE_GATEWAY: &str = "10.42.0.254";
pub const FIREWALL_CHAIN: &str = "MEGASERVER-FORWARD";
pub const INPUT_CHAIN: &str = "MEGASERVER-INPUT";

#[cfg(target_os = "linux")]
pub mod linux {
    use super::{
        BRIDGE_CIDR, BRIDGE_GATEWAY, BRIDGE_NAME, FIREWALL_CHAIN, INPUT_CHAIN, SANDBOX_SUBNET,
    };
    use anyhow::{Context, Result, bail};
    use std::collections::hash_map::DefaultHasher;
    use std::fs;
    use std::hash::{Hash, Hasher};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    #[cfg(test)]
    use std::collections::HashMap;
    #[cfg(test)]
    use std::sync::{Arc, Mutex};

    pub fn isolation_supported() -> bool {
        is_effective_root()
            && command_available("ip")
            && command_available("iptables")
            && Path::new("/sys/fs/cgroup").exists()
    }

    pub fn setup_sandbox_network(service_name: &str, sandbox_ip: &str) -> Result<()> {
        HostNetworkManager::new(SystemRunner).setup_sandbox_network(service_name, sandbox_ip)
    }

    pub fn cleanup_sandbox_network(service_name: Option<&str>) -> Result<()> {
        HostNetworkManager::new(SystemRunner).cleanup_sandbox_network(service_name)
    }

    pub fn sandbox_network_present(service_name: &str) -> Result<bool> {
        let host_veth = host_veth_name(service_name);
        let netns = netns_name(service_name);
        let host_exists = Command::new("ip")
            .args(["link", "show", &host_veth])
            .output()
            .with_context(|| format!("inspect host veth `{host_veth}`"))?
            .status
            .success();
        let netns_output = Command::new("ip")
            .args(["netns", "list"])
            .output()
            .context("list network namespaces")?;
        let netns_exists = if netns_output.status.success() {
            String::from_utf8_lossy(&netns_output.stdout)
                .lines()
                .any(|line| line.split_whitespace().next() == Some(netns.as_str()))
        } else {
            false
        };
        Ok(host_exists || netns_exists)
    }

    pub fn netns_name(service_name: &str) -> String {
        format!("ms-{}", short_id(service_name))
    }

    pub fn host_veth_name(service_name: &str) -> String {
        format!("msh{}", short_id(service_name))
    }

    pub fn guest_veth_name(service_name: &str) -> String {
        format!("msg{}", short_id(service_name))
    }

    trait Runner: Clone + Send + Sync + 'static {
        fn run(&self, program: &str, args: &[&str]) -> Result<String>;
        fn write_file(&self, path: &Path, contents: &str) -> Result<()>;
    }

    #[derive(Clone)]
    struct SystemRunner;

    impl Runner for SystemRunner {
        fn run(&self, program: &str, args: &[&str]) -> Result<String> {
            let output = Command::new(program)
                .args(args)
                .output()
                .with_context(|| format!("run `{program} {}`", args.join(" ")))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                bail!("`{program} {}` failed: {stderr}", args.join(" "));
            }
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        }

        fn write_file(&self, path: &Path, contents: &str) -> Result<()> {
            fs::write(path, contents).with_context(|| format!("write {}", path.display()))
        }
    }

    #[derive(Clone)]
    struct HostNetworkManager<R> {
        runner: R,
        netns_root: PathBuf,
    }

    impl<R: Runner> HostNetworkManager<R> {
        fn new(runner: R) -> Self {
            Self {
                runner,
                netns_root: PathBuf::from("/var/run/netns"),
            }
        }

        fn setup_sandbox_network(&self, service_name: &str, sandbox_ip: &str) -> Result<()> {
            fs::create_dir_all(&self.netns_root)
                .with_context(|| format!("create {}", self.netns_root.display()))?;
            self.ensure_host_network()?;
            self.cleanup_sandbox_network(Some(service_name))?;

            let netns_name = netns_name(service_name);
            let host_veth = host_veth_name(service_name);
            let guest_veth = guest_veth_name(service_name);

            self.ip(&["netns", "add", &netns_name])?;
            self.ip(&[
                "link",
                "add",
                &host_veth,
                "type",
                "veth",
                "peer",
                "name",
                &guest_veth,
            ])?;
            self.ip(&["link", "set", &host_veth, "master", BRIDGE_NAME])?;
            self.ip(&["link", "set", &host_veth, "up"])?;
            self.ip(&["link", "set", &guest_veth, "netns", &netns_name])?;
            self.ip_netns(&netns_name, &["ip", "link", "set", "lo", "up"])?;
            self.ip_netns(
                &netns_name,
                &["ip", "link", "set", &guest_veth, "name", "eth0"],
            )?;
            self.ip_netns(
                &netns_name,
                &[
                    "ip",
                    "addr",
                    "replace",
                    &format!("{sandbox_ip}/24"),
                    "dev",
                    "eth0",
                ],
            )?;
            self.ip_netns(&netns_name, &["ip", "link", "set", "eth0", "up"])?;
            self.ip_netns(
                &netns_name,
                &["ip", "route", "replace", "default", "via", BRIDGE_GATEWAY],
            )?;
            Ok(())
        }

        #[cfg(test)]
        fn with_netns_root(mut self, netns_root: PathBuf) -> Self {
            self.netns_root = netns_root;
            self
        }

        fn cleanup_sandbox_network(&self, service_name: Option<&str>) -> Result<()> {
            let Some(service_name) = service_name else {
                return Ok(());
            };
            let netns_name = netns_name(service_name);
            let host_veth = host_veth_name(service_name);

            let _ = self.ip_allow_failure(&["link", "del", &host_veth]);
            let _ = self.ip_allow_failure(&["netns", "del", &netns_name]);
            Ok(())
        }

        fn ensure_host_network(&self) -> Result<()> {
            if self
                .ip_allow_failure(&["link", "show", BRIDGE_NAME])
                .is_err()
            {
                self.ip(&["link", "add", BRIDGE_NAME, "type", "bridge"])?;
            }
            let _ = self.ip_allow_failure(&["addr", "replace", BRIDGE_CIDR, "dev", BRIDGE_NAME]);
            self.ip(&["link", "set", BRIDGE_NAME, "up"])?;

            self.runner
                .write_file(Path::new("/proc/sys/net/ipv4/ip_forward"), "1\n")
                .context("enable ipv4 forwarding")?;

            let uplink = self.default_uplink_interface()?;
            self.ensure_iptables_chain(FIREWALL_CHAIN, "FORWARD")?;
            self.ensure_iptables_chain(INPUT_CHAIN, "INPUT")?;
            self.ensure_iptables_rule(
                &["-C", "FORWARD", "-j", FIREWALL_CHAIN],
                &["-A", "FORWARD", "-j", FIREWALL_CHAIN],
            )?;
            self.ensure_iptables_rule(
                &[
                    "-C",
                    FIREWALL_CHAIN,
                    "-i",
                    BRIDGE_NAME,
                    "-o",
                    BRIDGE_NAME,
                    "-j",
                    "ACCEPT",
                ],
                &[
                    "-A",
                    FIREWALL_CHAIN,
                    "-i",
                    BRIDGE_NAME,
                    "-o",
                    BRIDGE_NAME,
                    "-j",
                    "ACCEPT",
                ],
            )?;
            self.ensure_iptables_rule(
                &["-C", FIREWALL_CHAIN, "-i", BRIDGE_NAME, "-j", "ACCEPT"],
                &["-A", FIREWALL_CHAIN, "-i", BRIDGE_NAME, "-j", "ACCEPT"],
            )?;
            self.ensure_iptables_rule(
                &[
                    "-C",
                    FIREWALL_CHAIN,
                    "-o",
                    BRIDGE_NAME,
                    "-m",
                    "conntrack",
                    "--ctstate",
                    "RELATED,ESTABLISHED",
                    "-j",
                    "ACCEPT",
                ],
                &[
                    "-A",
                    FIREWALL_CHAIN,
                    "-o",
                    BRIDGE_NAME,
                    "-m",
                    "conntrack",
                    "--ctstate",
                    "RELATED,ESTABLISHED",
                    "-j",
                    "ACCEPT",
                ],
            )?;
            self.ensure_iptables_rule(
                &["-C", FIREWALL_CHAIN, "-d", SANDBOX_SUBNET, "-j", "DROP"],
                &["-A", FIREWALL_CHAIN, "-d", SANDBOX_SUBNET, "-j", "DROP"],
            )?;
            self.ensure_iptables_rule(
                &["-C", "INPUT", "-j", INPUT_CHAIN],
                &["-A", "INPUT", "-j", INPUT_CHAIN],
            )?;
            self.ensure_iptables_rule(
                &[
                    "-C",
                    INPUT_CHAIN,
                    "-i",
                    BRIDGE_NAME,
                    "-p",
                    "udp",
                    "--dport",
                    "53",
                    "-j",
                    "ACCEPT",
                ],
                &[
                    "-A",
                    INPUT_CHAIN,
                    "-i",
                    BRIDGE_NAME,
                    "-p",
                    "udp",
                    "--dport",
                    "53",
                    "-j",
                    "ACCEPT",
                ],
            )?;
            self.ensure_iptables_rule(
                &["-C", INPUT_CHAIN, "-i", BRIDGE_NAME, "-j", "RETURN"],
                &["-A", INPUT_CHAIN, "-i", BRIDGE_NAME, "-j", "RETURN"],
            )?;
            self.ensure_iptables_rule(
                &[
                    "-t",
                    "nat",
                    "-C",
                    "POSTROUTING",
                    "-s",
                    SANDBOX_SUBNET,
                    "-o",
                    &uplink,
                    "-j",
                    "MASQUERADE",
                ],
                &[
                    "-t",
                    "nat",
                    "-A",
                    "POSTROUTING",
                    "-s",
                    SANDBOX_SUBNET,
                    "-o",
                    &uplink,
                    "-j",
                    "MASQUERADE",
                ],
            )?;
            Ok(())
        }

        fn default_uplink_interface(&self) -> Result<String> {
            let route = self.ip(&["route", "show", "default"])?;
            parse_default_interface(&route)
        }

        fn ensure_iptables_rule(&self, check_args: &[&str], add_args: &[&str]) -> Result<()> {
            if self.runner.run("iptables", check_args).is_err() {
                self.runner.run("iptables", add_args)?;
            }
            Ok(())
        }

        fn ensure_iptables_chain(&self, chain: &str, parent: &str) -> Result<()> {
            let _ = self.runner.run("iptables", &["-N", chain]);
            self.ensure_iptables_rule(&["-C", parent, "-j", chain], &["-A", parent, "-j", chain])
        }

        fn ip(&self, args: &[&str]) -> Result<String> {
            self.runner.run("ip", args)
        }

        fn ip_allow_failure(&self, args: &[&str]) -> Result<String> {
            self.runner.run("ip", args)
        }

        fn ip_netns(&self, netns: &str, args: &[&str]) -> Result<String> {
            let mut full_args = vec!["netns", "exec", netns];
            full_args.extend_from_slice(args);
            self.runner.run("ip", &full_args)
        }
    }

    fn short_id(service_name: &str) -> String {
        let mut hasher = DefaultHasher::new();
        service_name.hash(&mut hasher);
        format!("{:08x}", hasher.finish() as u32)
    }

    fn parse_default_interface(route_output: &str) -> Result<String> {
        let tokens = route_output.split_whitespace().collect::<Vec<_>>();
        for window in tokens.windows(2) {
            if window[0] == "dev" {
                return Ok(window[1].to_string());
            }
        }
        bail!("could not determine default interface from `ip route show default`")
    }

    fn is_effective_root() -> bool {
        fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|content| {
                content.lines().find_map(|line| {
                    let value = line.strip_prefix("Uid:")?;
                    value.split_whitespace().next()?.parse::<u32>().ok()
                })
            })
            == Some(0)
    }

    fn command_available(binary: &str) -> bool {
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
            .map(|dir| dir.join(binary))
            .any(|path| path.is_file())
    }

    #[cfg(test)]
    #[derive(Clone, Default)]
    struct FakeRunner {
        state: Arc<Mutex<FakeState>>,
    }

    #[cfg(test)]
    #[derive(Default)]
    struct FakeState {
        calls: Vec<String>,
        outputs: HashMap<String, Result<String, String>>,
        writes: Vec<(PathBuf, String)>,
    }

    #[cfg(test)]
    impl FakeRunner {
        fn with_output(self, command: &str, output: Result<&str, &str>) -> Self {
            let mut state = self.state.lock().unwrap();
            state.outputs.insert(
                command.to_string(),
                output
                    .map(|ok| ok.to_string())
                    .map_err(|err| err.to_string()),
            );
            drop(state);
            self
        }

        fn calls(&self) -> Vec<String> {
            self.state.lock().unwrap().calls.clone()
        }

        fn writes(&self) -> Vec<(PathBuf, String)> {
            self.state.lock().unwrap().writes.clone()
        }
    }

    #[cfg(test)]
    impl Runner for FakeRunner {
        fn run(&self, program: &str, args: &[&str]) -> Result<String> {
            let command = format!("{program} {}", args.join(" "));
            let mut state = self.state.lock().unwrap();
            state.calls.push(command.clone());
            match state.outputs.get(&command) {
                Some(Ok(output)) => Ok(output.clone()),
                Some(Err(message)) => bail!("{message}"),
                None => Ok(String::new()),
            }
        }

        fn write_file(&self, path: &Path, contents: &str) -> Result<()> {
            self.state
                .lock()
                .unwrap()
                .writes
                .push((path.to_path_buf(), contents.to_string()));
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_default_interface_from_route_output() {
            let route = "default via 172.17.0.1 dev eth0 proto dhcp src 172.17.0.2";
            assert_eq!(parse_default_interface(route).unwrap(), "eth0");
        }

        #[test]
        fn ensures_host_network_with_nat_and_forwarding() {
            let runner = FakeRunner::default()
                .with_output("ip link show megabr0", Err("missing bridge"))
                .with_output(
                    "ip route show default",
                    Ok("default via 172.17.0.1 dev eth0 proto dhcp src 172.17.0.2"),
                )
                .with_output(
                    "iptables -C FORWARD -j MEGASERVER-FORWARD",
                    Err("missing forward jump"),
                )
                .with_output(
                    "iptables -C MEGASERVER-FORWARD -i megabr0 -o megabr0 -j ACCEPT",
                    Err("missing east-west rule"),
                )
                .with_output(
                    "iptables -C MEGASERVER-FORWARD -i megabr0 -j ACCEPT",
                    Err("missing outbound rule"),
                )
                .with_output(
                    "iptables -C MEGASERVER-FORWARD -o megabr0 -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT",
                    Err("missing established rule"),
                )
                .with_output(
                    "iptables -C MEGASERVER-FORWARD -d 10.42.0.0/24 -j DROP",
                    Err("missing drop rule"),
                )
                .with_output(
                    "iptables -C INPUT -j MEGASERVER-INPUT",
                    Err("missing input jump"),
                )
                .with_output(
                    "iptables -C MEGASERVER-INPUT -i megabr0 -p udp --dport 53 -j ACCEPT",
                    Err("missing dns rule"),
                )
                .with_output(
                    "iptables -C MEGASERVER-INPUT -i megabr0 -j RETURN",
                    Err("missing return rule"),
                )
                .with_output(
                    "iptables -t nat -C POSTROUTING -s 10.42.0.0/24 -o eth0 -j MASQUERADE",
                    Err("missing nat rule"),
                );
            let manager = HostNetworkManager::new(runner.clone());

            manager.ensure_host_network().unwrap();

            let calls = runner.calls();
            assert!(calls.contains(&"ip link add megabr0 type bridge".to_string()));
            assert!(calls.contains(&"ip addr replace 10.42.0.254/24 dev megabr0".to_string()));
            assert!(calls.contains(
                &"iptables -A MEGASERVER-FORWARD -i megabr0 -o megabr0 -j ACCEPT".to_string()
            ));
            assert!(
                calls.contains(&"iptables -A MEGASERVER-FORWARD -i megabr0 -j ACCEPT".to_string())
            );
            assert!(calls.contains(
                &"iptables -A MEGASERVER-FORWARD -o megabr0 -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT".to_string()
            ));
            assert!(
                calls.contains(
                    &"iptables -A MEGASERVER-FORWARD -d 10.42.0.0/24 -j DROP".to_string()
                )
            );
            assert!(calls.contains(
                &"iptables -A MEGASERVER-INPUT -i megabr0 -p udp --dport 53 -j ACCEPT".to_string()
            ));
            assert!(
                calls.contains(&"iptables -A MEGASERVER-INPUT -i megabr0 -j RETURN".to_string())
            );
            assert!(calls.contains(
                &"iptables -t nat -A POSTROUTING -s 10.42.0.0/24 -o eth0 -j MASQUERADE".to_string()
            ));
            assert_eq!(
                runner.writes(),
                vec![(
                    PathBuf::from("/proc/sys/net/ipv4/ip_forward"),
                    "1\n".to_string()
                )]
            );
        }

        #[test]
        fn configures_netns_and_veth_for_service() {
            let runner = FakeRunner::default()
                .with_output(
                    "ip route show default",
                    Ok("default via 172.17.0.1 dev eth0 proto dhcp src 172.17.0.2"),
                )
                .with_output(
                    "iptables -C FORWARD -j MEGASERVER-FORWARD",
                    Ok(""),
                )
                .with_output(
                    "iptables -C MEGASERVER-FORWARD -i megabr0 -o megabr0 -j ACCEPT",
                    Ok(""),
                )
                .with_output(
                    "iptables -C MEGASERVER-FORWARD -i megabr0 -j ACCEPT",
                    Ok(""),
                )
                .with_output(
                    "iptables -C MEGASERVER-FORWARD -o megabr0 -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT",
                    Ok(""),
                )
                .with_output(
                    "iptables -C MEGASERVER-FORWARD -d 10.42.0.0/24 -j DROP",
                    Ok(""),
                )
                .with_output(
                    "iptables -C INPUT -j MEGASERVER-INPUT",
                    Ok(""),
                )
                .with_output(
                    "iptables -C MEGASERVER-INPUT -i megabr0 -p udp --dport 53 -j ACCEPT",
                    Ok(""),
                )
                .with_output(
                    "iptables -C MEGASERVER-INPUT -i megabr0 -j RETURN",
                    Ok(""),
                )
                .with_output(
                    "iptables -t nat -C POSTROUTING -s 10.42.0.0/24 -o eth0 -j MASQUERADE",
                    Ok(""),
                );
            let temp = tempfile::TempDir::new().unwrap();
            let manager =
                HostNetworkManager::new(runner.clone()).with_netns_root(temp.path().join("netns"));

            manager
                .setup_sandbox_network("hello-service", "10.42.0.10")
                .unwrap();

            let netns = netns_name("hello-service");
            let host_veth = host_veth_name("hello-service");
            let guest_veth = guest_veth_name("hello-service");
            let calls = runner.calls();
            assert!(calls.contains(&format!("ip netns add {netns}")));
            assert!(calls.contains(&format!(
                "ip link add {host_veth} type veth peer name {guest_veth}"
            )));
            assert!(calls.contains(&format!("ip link set {guest_veth} netns {netns}")));
            assert!(calls.contains(&format!(
                "ip netns exec {netns} ip addr replace 10.42.0.10/24 dev eth0"
            )));
            assert!(calls.contains(&format!(
                "ip netns exec {netns} ip route replace default via 10.42.0.254"
            )));
        }
    }
}

pub fn sandbox_gateway() -> &'static str {
    BRIDGE_GATEWAY
}

pub fn sandbox_subnet() -> &'static str {
    SANDBOX_SUBNET
}

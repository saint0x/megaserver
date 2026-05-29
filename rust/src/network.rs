use crate::manifest::NetworkPolicySection;

pub const SANDBOX_SUBNET: &str = "10.42.0.0/24";
pub const BRIDGE_NAME: &str = "megabr0";
pub const BRIDGE_CIDR: &str = "10.42.0.254/24";
pub const BRIDGE_GATEWAY: &str = "10.42.0.254";
pub const FIREWALL_CHAIN: &str = "MEGASERVER-FORWARD";
pub const INPUT_CHAIN: &str = "MEGASERVER-INPUT";
pub const POLICY_CHAIN: &str = "MEGASERVER-POLICY";

#[derive(Debug, Clone)]
pub struct SandboxNetworkPolicyBinding {
    pub service_name: String,
    pub sandbox_ip: String,
    pub policy: NetworkPolicySection,
}

#[cfg(target_os = "linux")]
pub mod linux {
    use super::{
        BRIDGE_CIDR, BRIDGE_GATEWAY, BRIDGE_NAME, FIREWALL_CHAIN, INPUT_CHAIN, POLICY_CHAIN,
        SANDBOX_SUBNET, SandboxNetworkPolicyBinding,
    };
    use crate::manifest::{NetworkAccessPolicy, NetworkPolicyDefault, NetworkPolicySection};
    use anyhow::{Context, Result, bail};
    use ipnet::IpNet;
    use std::collections::hash_map::DefaultHasher;
    use std::fs;
    use std::hash::{Hash, Hasher};
    use std::net::IpAddr;
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

    pub fn refresh_sandbox_policies(bindings: &[SandboxNetworkPolicyBinding]) -> Result<()> {
        HostNetworkManager::new(SystemRunner).refresh_sandbox_policies(bindings)
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

        fn refresh_sandbox_policies(&self, bindings: &[SandboxNetworkPolicyBinding]) -> Result<()> {
            self.ensure_host_network()?;
            self.iptables(&["-F", POLICY_CHAIN])?;
            for rule in compile_policy_rules(bindings)? {
                let args = rule.iter().map(String::as_str).collect::<Vec<_>>();
                self.iptables(&args)?;
            }
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
            self.ensure_iptables_chain(POLICY_CHAIN, FIREWALL_CHAIN)?;
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
                    "!",
                    "-s",
                    SANDBOX_SUBNET,
                    "-j",
                    "DROP",
                ],
                &[
                    "-A",
                    FIREWALL_CHAIN,
                    "-i",
                    BRIDGE_NAME,
                    "!",
                    "-s",
                    SANDBOX_SUBNET,
                    "-j",
                    "DROP",
                ],
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
                &["-C", FIREWALL_CHAIN, "-j", POLICY_CHAIN],
                &["-A", FIREWALL_CHAIN, "-j", POLICY_CHAIN],
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
                    "!",
                    "-s",
                    SANDBOX_SUBNET,
                    "-j",
                    "DROP",
                ],
                &[
                    "-A",
                    INPUT_CHAIN,
                    "-i",
                    BRIDGE_NAME,
                    "!",
                    "-s",
                    SANDBOX_SUBNET,
                    "-j",
                    "DROP",
                ],
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
                &[
                    "-C",
                    INPUT_CHAIN,
                    "-i",
                    BRIDGE_NAME,
                    "-p",
                    "tcp",
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
                    "tcp",
                    "--dport",
                    "53",
                    "-j",
                    "ACCEPT",
                ],
            )?;
            self.ensure_iptables_rule(
                &["-C", INPUT_CHAIN, "-i", BRIDGE_NAME, "-j", "DROP"],
                &["-A", INPUT_CHAIN, "-i", BRIDGE_NAME, "-j", "DROP"],
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

        fn iptables(&self, args: &[&str]) -> Result<String> {
            self.runner.run("iptables", args)
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

    fn compile_policy_rules(bindings: &[SandboxNetworkPolicyBinding]) -> Result<Vec<Vec<String>>> {
        let mut rules = Vec::new();
        let subnet: IpNet = SANDBOX_SUBNET.parse().context("parse sandbox subnet")?;
        let compiled = bindings
            .iter()
            .map(CompiledBinding::new)
            .collect::<Result<Vec<_>>>()?;

        for source in &compiled {
            for target in &compiled {
                if source.service_name == target.service_name {
                    continue;
                }
                let allowed =
                    policy_allows(&source.policy.egress, Some(&target.service_name), target.ip)
                        && policy_allows(
                            &target.policy.ingress,
                            Some(&source.service_name),
                            source.ip,
                        );
                rules.push(vec![
                    "-A".to_string(),
                    POLICY_CHAIN.to_string(),
                    "-s".to_string(),
                    source.ip_text.clone(),
                    "-d".to_string(),
                    target.ip_text.clone(),
                    "-j".to_string(),
                    if allowed { "ACCEPT" } else { "DROP" }.to_string(),
                ]);
            }
        }

        for target in &compiled {
            for cidr in &target.policy.ingress.deny_cidrs {
                let cidr = cidr
                    .parse::<IpNet>()
                    .with_context(|| format!("parse ingress CIDR `{cidr}`"))?;
                rules.push(rule_for_destination(&cidr, &target.ip_text, "DROP"));
            }
            for cidr in &target.policy.ingress.allow_cidrs {
                let cidr = cidr
                    .parse::<IpNet>()
                    .with_context(|| format!("parse ingress CIDR `{cidr}`"))?;
                rules.push(rule_for_destination(&cidr, &target.ip_text, "ACCEPT"));
            }
            if target.policy.ingress.default == NetworkPolicyDefault::Deny {
                rules.push(vec![
                    "-A".to_string(),
                    POLICY_CHAIN.to_string(),
                    "-d".to_string(),
                    target.ip_text.clone(),
                    "-j".to_string(),
                    "DROP".to_string(),
                ]);
            }
        }

        for source in &compiled {
            for cidr in &source.policy.egress.deny_cidrs {
                let cidr = cidr
                    .parse::<IpNet>()
                    .with_context(|| format!("parse egress CIDR `{cidr}`"))?;
                rules.push(rule_for_source(&source.ip_text, &cidr, "DROP"));
            }
            for cidr in &source.policy.egress.allow_cidrs {
                let cidr = cidr
                    .parse::<IpNet>()
                    .with_context(|| format!("parse egress CIDR `{cidr}`"))?;
                rules.push(rule_for_source(&source.ip_text, &cidr, "ACCEPT"));
            }
            rules.push(vec![
                "-A".to_string(),
                POLICY_CHAIN.to_string(),
                "-s".to_string(),
                source.ip_text.clone(),
                "!".to_string(),
                "-d".to_string(),
                subnet.to_string(),
                "-j".to_string(),
                match source.policy.egress.default {
                    NetworkPolicyDefault::Allow => "ACCEPT",
                    NetworkPolicyDefault::Deny => "DROP",
                }
                .to_string(),
            ]);
            rules.push(vec![
                "-A".to_string(),
                POLICY_CHAIN.to_string(),
                "-s".to_string(),
                source.ip_text.clone(),
                "-d".to_string(),
                subnet.to_string(),
                "-j".to_string(),
                "DROP".to_string(),
            ]);
        }

        Ok(rules)
    }

    fn rule_for_source(source_ip: &str, cidr: &IpNet, action: &str) -> Vec<String> {
        vec![
            "-A".to_string(),
            POLICY_CHAIN.to_string(),
            "-s".to_string(),
            source_ip.to_string(),
            "-d".to_string(),
            cidr.to_string(),
            "-j".to_string(),
            action.to_string(),
        ]
    }

    fn rule_for_destination(source_cidr: &IpNet, target_ip: &str, action: &str) -> Vec<String> {
        vec![
            "-A".to_string(),
            POLICY_CHAIN.to_string(),
            "-s".to_string(),
            source_cidr.to_string(),
            "-d".to_string(),
            target_ip.to_string(),
            "-j".to_string(),
            action.to_string(),
        ]
    }

    fn policy_allows(
        policy: &NetworkAccessPolicy,
        service_name: Option<&str>,
        peer_ip: IpAddr,
    ) -> bool {
        if policy
            .deny_services
            .iter()
            .any(|candidate| Some(candidate.as_str()) == service_name)
        {
            return false;
        }
        if policy
            .deny_cidrs
            .iter()
            .filter_map(|cidr| cidr.parse::<IpNet>().ok())
            .any(|cidr| cidr.contains(&peer_ip))
        {
            return false;
        }
        if policy
            .allow_services
            .iter()
            .any(|candidate| Some(candidate.as_str()) == service_name)
        {
            return true;
        }
        if policy
            .allow_cidrs
            .iter()
            .filter_map(|cidr| cidr.parse::<IpNet>().ok())
            .any(|cidr| cidr.contains(&peer_ip))
        {
            return true;
        }
        matches!(policy.default, NetworkPolicyDefault::Allow)
    }

    struct CompiledBinding<'a> {
        service_name: &'a str,
        ip: IpAddr,
        ip_text: String,
        policy: &'a NetworkPolicySection,
    }

    impl<'a> CompiledBinding<'a> {
        fn new(binding: &'a SandboxNetworkPolicyBinding) -> Result<Self> {
            let ip: IpAddr = binding
                .sandbox_ip
                .parse()
                .with_context(|| format!("parse sandbox ip for {}", binding.service_name))?;
            Ok(Self {
                service_name: &binding.service_name,
                ip,
                ip_text: binding.sandbox_ip.clone(),
                policy: &binding.policy,
            })
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
                    "iptables -C MEGASERVER-FORWARD -i megabr0 ! -s 10.42.0.0/24 -j DROP",
                    Err("missing anti-spoof forward rule"),
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
                    "iptables -C MEGASERVER-FORWARD -j MEGASERVER-POLICY",
                    Err("missing policy jump"),
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
                    "iptables -C MEGASERVER-INPUT -i megabr0 ! -s 10.42.0.0/24 -j DROP",
                    Err("missing anti-spoof input rule"),
                )
                .with_output(
                    "iptables -C MEGASERVER-INPUT -i megabr0 -p udp --dport 53 -j ACCEPT",
                    Err("missing dns rule"),
                )
                .with_output(
                    "iptables -C MEGASERVER-INPUT -i megabr0 -p tcp --dport 53 -j ACCEPT",
                    Err("missing tcp dns rule"),
                )
                .with_output(
                    "iptables -C MEGASERVER-INPUT -i megabr0 -j DROP",
                    Err("missing input drop rule"),
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
                &"iptables -A MEGASERVER-FORWARD -i megabr0 ! -s 10.42.0.0/24 -j DROP".to_string()
            ));
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
                calls.contains(&"iptables -A MEGASERVER-FORWARD -j MEGASERVER-POLICY".to_string())
            );
            assert!(
                calls.contains(
                    &"iptables -A MEGASERVER-FORWARD -d 10.42.0.0/24 -j DROP".to_string()
                )
            );
            assert!(calls.contains(
                &"iptables -A MEGASERVER-INPUT -i megabr0 ! -s 10.42.0.0/24 -j DROP".to_string()
            ));
            assert!(calls.contains(
                &"iptables -A MEGASERVER-INPUT -i megabr0 -p udp --dport 53 -j ACCEPT".to_string()
            ));
            assert!(calls.contains(
                &"iptables -A MEGASERVER-INPUT -i megabr0 -p tcp --dport 53 -j ACCEPT".to_string()
            ));
            assert!(calls.contains(&"iptables -A MEGASERVER-INPUT -i megabr0 -j DROP".to_string()));
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
                    "iptables -C MEGASERVER-FORWARD -i megabr0 ! -s 10.42.0.0/24 -j DROP",
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
                .with_output("iptables -C MEGASERVER-FORWARD -j MEGASERVER-POLICY", Ok(""))
                .with_output(
                    "iptables -C MEGASERVER-FORWARD -d 10.42.0.0/24 -j DROP",
                    Ok(""),
                )
                .with_output(
                    "iptables -C INPUT -j MEGASERVER-INPUT",
                    Ok(""),
                )
                .with_output(
                    "iptables -C MEGASERVER-INPUT -i megabr0 ! -s 10.42.0.0/24 -j DROP",
                    Ok(""),
                )
                .with_output(
                    "iptables -C MEGASERVER-INPUT -i megabr0 -p udp --dport 53 -j ACCEPT",
                    Ok(""),
                )
                .with_output(
                    "iptables -C MEGASERVER-INPUT -i megabr0 -p tcp --dport 53 -j ACCEPT",
                    Ok(""),
                )
                .with_output(
                    "iptables -C MEGASERVER-INPUT -i megabr0 -j DROP",
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

        #[test]
        fn compiles_service_network_policies() {
            let rules = compile_policy_rules(&[
                SandboxNetworkPolicyBinding {
                    service_name: "frontend".to_string(),
                    sandbox_ip: "10.42.0.10".to_string(),
                    policy: NetworkPolicySection::default(),
                },
                SandboxNetworkPolicyBinding {
                    service_name: "backend".to_string(),
                    sandbox_ip: "10.42.0.11".to_string(),
                    policy: NetworkPolicySection {
                        ingress: NetworkAccessPolicy {
                            default: NetworkPolicyDefault::Deny,
                            allow_services: vec!["frontend".to_string()],
                            allow_cidrs: vec![],
                            deny_services: vec![],
                            deny_cidrs: vec![],
                        },
                        egress: NetworkAccessPolicy::default(),
                    },
                },
                SandboxNetworkPolicyBinding {
                    service_name: "worker".to_string(),
                    sandbox_ip: "10.42.0.12".to_string(),
                    policy: NetworkPolicySection {
                        ingress: NetworkAccessPolicy::default(),
                        egress: NetworkAccessPolicy {
                            default: NetworkPolicyDefault::Deny,
                            allow_services: vec![],
                            allow_cidrs: vec!["8.8.8.8/32".to_string()],
                            deny_services: vec!["backend".to_string()],
                            deny_cidrs: vec!["169.254.169.254/32".to_string()],
                        },
                    },
                },
            ])
            .unwrap()
            .into_iter()
            .map(|rule| rule.join(" "))
            .collect::<Vec<_>>();

            assert!(rules.contains(
                &"-A MEGASERVER-POLICY -s 10.42.0.10 -d 10.42.0.11 -j ACCEPT".to_string()
            ));
            assert!(
                rules.contains(
                    &"-A MEGASERVER-POLICY -s 10.42.0.12 -d 10.42.0.11 -j DROP".to_string()
                )
            );
            assert!(rules.contains(
                &"-A MEGASERVER-POLICY -s 10.42.0.12 -d 169.254.169.254/32 -j DROP".to_string()
            ));
            assert!(rules.contains(
                &"-A MEGASERVER-POLICY -s 10.42.0.12 -d 8.8.8.8/32 -j ACCEPT".to_string()
            ));
            assert!(rules.contains(
                &"-A MEGASERVER-POLICY -s 10.42.0.12 ! -d 10.42.0.0/24 -j DROP".to_string()
            ));
            assert!(rules.contains(&"-A MEGASERVER-POLICY -d 10.42.0.11 -j DROP".to_string()));
        }
    }
}

pub fn sandbox_gateway() -> &'static str {
    BRIDGE_GATEWAY
}

pub fn sandbox_subnet() -> &'static str {
    SANDBOX_SUBNET
}

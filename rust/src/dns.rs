use crate::state::{self, StatePaths};
use anyhow::{Context, Result, anyhow, bail};
use std::fs;
#[cfg(not(test))]
use std::fs::OpenOptions;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::Path;
#[cfg(not(test))]
use std::process::{Command, Stdio};
#[cfg(test)]
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const DNS_PID_FILE: &str = "dns.pid";
#[cfg(not(test))]
const DNS_LOG_FILE: &str = "dns.log";

#[cfg(test)]
pub fn ensure_running(paths: &StatePaths) -> Result<()> {
    if !cfg!(target_os = "linux") || !crate::network::linux::isolation_supported() {
        return Ok(());
    }
    start_embedded_test_server(paths.clone())
}

#[cfg(not(test))]
pub fn ensure_running(paths: &StatePaths) -> Result<()> {
    if !cfg!(target_os = "linux") || !crate::network::linux::isolation_supported() {
        return Ok(());
    }
    let pid_path = pid_path(paths);
    if let Some(pid) = read_pid(&pid_path)? {
        if crate::runtime::process_alive(pid) {
            return Ok(());
        }
        let _ = fs::remove_file(&pid_path);
    }

    let current_exe = std::env::current_exe().context("resolve current executable")?;
    let log_path = paths.logs_dir.join(DNS_LOG_FILE);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;
    let bind = format!("{}:53", crate::network::sandbox_gateway());
    let child = Command::new(current_exe)
        .arg("--home")
        .arg(&paths.home)
        .arg("dns")
        .arg("--bind")
        .arg(&bind)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()
        .context("spawn megaserver dns sidecar")?;

    fs::write(&pid_path, child.id().to_string())
        .with_context(|| format!("write {}", pid_path.display()))?;

    wait_until_ready(&bind)?;
    Ok(())
}

pub fn stop_if_idle(paths: &StatePaths) -> Result<()> {
    let conn = state::open(paths)?;
    let in_use = state::list_sandboxes(&conn)?.into_iter().any(|sandbox| {
        sandbox.runtime_kind == "linux-namespace"
            && sandbox.status != "stopped"
            && sandbox.pid.is_some()
    });
    drop(conn);
    if in_use {
        return Ok(());
    }

    let pid_path = pid_path(paths);
    if let Some(pid) = read_pid(&pid_path)? {
        let _ = crate::runtime::stop_pid(pid, Duration::from_secs(2));
    }
    let _ = fs::remove_file(pid_path);
    Ok(())
}

pub fn serve_forever(paths: StatePaths, bind: SocketAddr) -> Result<()> {
    let socket = UdpSocket::bind(bind).with_context(|| format!("bind dns socket {bind}"))?;
    let mut buffer = [0_u8; 512];
    loop {
        let (len, peer) = socket.recv_from(&mut buffer)?;
        if let Some(response) = respond(&paths, &buffer[..len])? {
            let _ = socket.send_to(&response, peer);
        }
    }
}

fn wait_until_ready(bind: &str) -> Result<()> {
    let addr: SocketAddr = bind.parse().context("parse dns bind addr")?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if UdpSocket::bind("0.0.0.0:0")
            .and_then(|socket| {
                socket.set_read_timeout(Some(Duration::from_millis(200)))?;
                let query = build_a_query("gateway.megaserver");
                socket.send_to(&query, addr)?;
                let mut response = [0_u8; 512];
                let _ = socket.recv_from(&mut response)?;
                Ok(())
            })
            .is_ok()
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("dns sidecar did not become ready on {bind}");
}

fn pid_path(paths: &StatePaths) -> std::path::PathBuf {
    paths.run_dir.join(DNS_PID_FILE)
}

fn read_pid(path: &Path) -> Result<Option<i32>> {
    let Ok(raw) = fs::read_to_string(path) else {
        return Ok(None);
    };
    let pid = raw.trim().parse::<i32>().context("parse dns pid")?;
    Ok(Some(pid))
}

fn respond(paths: &StatePaths, packet: &[u8]) -> Result<Option<Vec<u8>>> {
    let query = parse_query(packet)?;
    if query.qtype != 1 || query.qclass != 1 {
        return Ok(Some(build_response(packet, query.end_offset, None, 4)));
    }
    let answer = lookup_name(paths, &query.name)?;
    let response = build_response(
        packet,
        query.end_offset,
        answer,
        if answer.is_some() { 0 } else { 3 },
    );
    Ok(Some(response))
}

fn lookup_name(paths: &StatePaths, name: &str) -> Result<Option<Ipv4Addr>> {
    if name.eq_ignore_ascii_case("gateway.megaserver") {
        return Ok(Some(
            crate::network::sandbox_gateway()
                .parse()
                .expect("valid sandbox gateway"),
        ));
    }

    let conn = state::open(paths)?;
    for sandbox in state::list_sandboxes(&conn)? {
        if sandbox.status == "stopped" {
            continue;
        }
        let Some(ip) = sandbox.ip_address.as_deref() else {
            continue;
        };
        let matches = name.eq_ignore_ascii_case(&sandbox.service_name)
            || name.eq_ignore_ascii_case(&sandbox.hostname);
        if matches {
            return Ok(Some(
                ip.parse()
                    .with_context(|| format!("parse sandbox ip {ip}"))?,
            ));
        }
    }
    Ok(None)
}

struct ParsedQuery {
    name: String,
    qtype: u16,
    qclass: u16,
    end_offset: usize,
}

fn parse_query(packet: &[u8]) -> Result<ParsedQuery> {
    if packet.len() < 12 {
        bail!("dns packet too short");
    }
    let qdcount = u16::from_be_bytes([packet[4], packet[5]]);
    if qdcount != 1 {
        bail!("expected one dns question");
    }
    let mut offset = 12;
    let mut labels = Vec::new();
    loop {
        let len = *packet
            .get(offset)
            .ok_or_else(|| anyhow!("dns label offset out of bounds"))? as usize;
        offset += 1;
        if len == 0 {
            break;
        }
        let end = offset + len;
        let label = packet
            .get(offset..end)
            .ok_or_else(|| anyhow!("dns label out of bounds"))?;
        labels.push(String::from_utf8_lossy(label).to_string());
        offset = end;
    }
    let qtype = u16::from_be_bytes([
        *packet.get(offset).ok_or_else(|| anyhow!("missing qtype"))?,
        *packet
            .get(offset + 1)
            .ok_or_else(|| anyhow!("missing qtype"))?,
    ]);
    let qclass = u16::from_be_bytes([
        *packet
            .get(offset + 2)
            .ok_or_else(|| anyhow!("missing qclass"))?,
        *packet
            .get(offset + 3)
            .ok_or_else(|| anyhow!("missing qclass"))?,
    ]);
    Ok(ParsedQuery {
        name: labels.join("."),
        qtype,
        qclass,
        end_offset: offset + 4,
    })
}

fn build_response(
    request: &[u8],
    question_end: usize,
    answer: Option<Ipv4Addr>,
    rcode: u8,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&request[0..2]);
    let rd = request[2] & 0x01;
    out.extend_from_slice(&[0x80 | rd, 0x80 | (rcode & 0x0f)]);
    out.extend_from_slice(&[0x00, 0x01]);
    let ancount = if answer.is_some() { 1_u16 } else { 0_u16 };
    out.extend_from_slice(&ancount.to_be_bytes());
    out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    out.extend_from_slice(&request[12..question_end]);
    if let Some(answer) = answer {
        out.extend_from_slice(&[0xc0, 0x0c]);
        out.extend_from_slice(&1_u16.to_be_bytes());
        out.extend_from_slice(&1_u16.to_be_bytes());
        out.extend_from_slice(&5_u32.to_be_bytes());
        out.extend_from_slice(&4_u16.to_be_bytes());
        out.extend_from_slice(&answer.octets());
    }
    out
}

fn build_a_query(name: &str) -> Vec<u8> {
    let mut out = vec![
        0x12, 0x34, // id
        0x01, 0x00, // flags
        0x00, 0x01, // qdcount
        0x00, 0x00, // ancount
        0x00, 0x00, // nscount
        0x00, 0x00, // arcount
    ];
    for label in name.split('.') {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    out.extend_from_slice(&1_u16.to_be_bytes());
    out.extend_from_slice(&1_u16.to_be_bytes());
    out
}

#[cfg(test)]
fn start_embedded_test_server(paths: StatePaths) -> Result<()> {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.get().is_none() {
        let bind: SocketAddr = format!("{}:53", crate::network::sandbox_gateway())
            .parse()
            .context("parse embedded dns bind")?;
        std::thread::spawn(move || {
            let _ = serve_forever(paths, bind);
        });
        STARTED.set(()).ok();
    }
    wait_until_ready(&format!("{}:53", crate::network::sandbox_gateway()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn dns_response_returns_gateway_a_record() {
        let temp = TempDir::new().unwrap();
        let paths = StatePaths::resolve(Some(temp.path().join("home"))).unwrap();
        state::init(&paths).unwrap();

        let packet = build_a_query("gateway.megaserver");
        let response = respond(&paths, &packet).unwrap().unwrap();
        assert_eq!(u16::from_be_bytes([response[6], response[7]]), 1);
        assert_eq!(
            &response[response.len() - 4..],
            &crate::network::sandbox_gateway()
                .parse::<Ipv4Addr>()
                .unwrap()
                .octets()
        );
    }

    #[test]
    fn dns_response_resolves_running_sandbox_names() {
        let temp = TempDir::new().unwrap();
        let paths = StatePaths::resolve(Some(temp.path().join("home"))).unwrap();
        state::init(&paths).unwrap();
        let conn = state::open(&paths).unwrap();
        state::upsert_sandbox(
            &conn,
            state::SandboxUpsert {
                service_name: "hello-service",
                sandbox_id: "hello-1",
                hostname: "hello-service.sandbox.megaserver",
                ip_address: Some("10.42.0.8"),
                runtime_kind: "linux-namespace",
                isolation_mode: "net+uts+mount+ipc+cgroup",
                status: "running",
                pid: Some(1),
                cgroup_path: Some("/sys/fs/cgroup/megaserver/hello"),
            },
        )
        .unwrap();

        let packet = build_a_query("hello-service");
        let response = respond(&paths, &packet).unwrap().unwrap();
        assert_eq!(&response[response.len() - 4..], &[10, 42, 0, 8]);
    }
}

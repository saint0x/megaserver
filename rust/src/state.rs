use crate::model::{
    EventRecord, RouteRecord, SandboxRecord, SecretRecord, ServiceRecord, SnapshotRecord,
    VolumeRecord,
};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::fs;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct StatePaths {
    pub home: PathBuf,
    pub db_path: PathBuf,
    pub logs_dir: PathBuf,
    pub run_dir: PathBuf,
    pub volumes_dir: PathBuf,
    pub snapshots_dir: PathBuf,
}

impl StatePaths {
    pub fn resolve(home_override: Option<PathBuf>) -> Result<Self> {
        let home = if let Some(home) = home_override {
            home
        } else if let Ok(home) = std::env::var("MEGASERVER_HOME") {
            PathBuf::from(home)
        } else {
            std::env::current_dir()?.join(".megaserver")
        };
        let db_path = home.join("state/megaserver.db");
        Ok(Self {
            logs_dir: home.join("logs"),
            run_dir: home.join("run"),
            volumes_dir: home.join("volumes"),
            snapshots_dir: home.join("snapshots"),
            home,
            db_path,
        })
    }

    pub fn ensure(&self) -> Result<()> {
        for dir in [
            &self.home,
            &self.logs_dir,
            &self.run_dir,
            &self.volumes_dir,
            &self.snapshots_dir,
            self.db_path.parent().expect("db parent"),
        ] {
            fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        Ok(())
    }

    pub fn service_logs_dir(&self, service: &str) -> PathBuf {
        self.logs_dir.join(service)
    }

    pub fn service_runtime_dir(&self, service: &str) -> PathBuf {
        self.run_dir.join(service)
    }
}

pub fn init(paths: &StatePaths) -> Result<()> {
    paths.ensure()?;
    let conn = open(paths)?;
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS services (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            status TEXT NOT NULL,
            app_path TEXT NOT NULL,
            manifest_json TEXT NOT NULL,
            plan_json TEXT NOT NULL,
            pid INTEGER,
            port INTEGER,
            health_path TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS deployments (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            service_name TEXT NOT NULL,
            manifest_json TEXT NOT NULL,
            plan_json TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS routes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            service_name TEXT NOT NULL,
            domain TEXT NOT NULL,
            port INTEGER,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS volumes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            service_name TEXT,
            name TEXT NOT NULL UNIQUE,
            host_path TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS secrets (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            service_name TEXT NOT NULL,
            key TEXT NOT NULL,
            value TEXT NOT NULL,
            created_at TEXT NOT NULL,
            UNIQUE(service_name, key)
        );
        CREATE TABLE IF NOT EXISTS snapshots (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            service_name TEXT NOT NULL,
            snapshot_name TEXT NOT NULL UNIQUE,
            snapshot_path TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            service_name TEXT,
            kind TEXT NOT NULL,
            details_json TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS sandboxes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            service_name TEXT NOT NULL UNIQUE,
            sandbox_id TEXT NOT NULL,
            hostname TEXT NOT NULL,
            ip_address TEXT,
            runtime_kind TEXT NOT NULL,
            isolation_mode TEXT NOT NULL,
            status TEXT NOT NULL,
            pid INTEGER,
            cgroup_path TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        ",
    )?;
    Ok(())
}

pub fn open(paths: &StatePaths) -> Result<Connection> {
    Connection::open(&paths.db_path)
        .with_context(|| format!("open sqlite db {}", paths.db_path.display()))
}

pub fn now_string() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{now}")
}

pub fn allocate_sandbox_ip(conn: &Connection, service_name: &str) -> Result<String> {
    if let Some(existing) = sandbox_by_service(conn, service_name)?
        && let Some(ip) = existing.ip_address
    {
        ip.parse::<Ipv4Addr>()
            .with_context(|| format!("parse stored sandbox ip for `{service_name}`"))?;
        return Ok(ip);
    }

    let subnet: ipnet::Ipv4Net = "10.42.0.0/24".parse().unwrap();
    let used = list_sandboxes(conn)?
        .into_iter()
        .filter_map(|sandbox| sandbox.ip_address)
        .filter_map(|ip| ip.parse::<Ipv4Addr>().ok())
        .collect::<std::collections::HashSet<_>>();
    for host in subnet.hosts() {
        if !used.contains(&host) {
            return Ok(host.to_string());
        }
    }
    anyhow::bail!("no available sandbox IPs in {}", subnet)
}

pub fn insert_deployment(
    conn: &Connection,
    service_name: &str,
    manifest_json: &str,
    plan_json: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO deployments(service_name, manifest_json, plan_json, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![service_name, manifest_json, plan_json, now_string()],
    )?;
    Ok(())
}

pub fn upsert_service(
    conn: &Connection,
    service_name: &str,
    status: &str,
    app_path: &Path,
    manifest_json: &str,
    plan_json: &str,
    port: Option<u16>,
    health_path: Option<&str>,
) -> Result<()> {
    let now = now_string();
    conn.execute(
        "
        INSERT INTO services(name, status, app_path, manifest_json, plan_json, pid, port, health_path, created_at, updated_at)
        VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, ?8, ?8)
        ON CONFLICT(name) DO UPDATE SET
            status = excluded.status,
            app_path = excluded.app_path,
            manifest_json = excluded.manifest_json,
            plan_json = excluded.plan_json,
            port = excluded.port,
            health_path = excluded.health_path,
            updated_at = excluded.updated_at
        ",
        params![
            service_name,
            status,
            app_path.to_string_lossy(),
            manifest_json,
            plan_json,
            port.map(i64::from),
            health_path,
            now,
        ],
    )?;
    Ok(())
}

pub fn service_by_name(conn: &Connection, service_name: &str) -> Result<Option<ServiceRecord>> {
    conn.query_row(
        "
        SELECT id, name, status, app_path, manifest_json, plan_json, pid, port, health_path, created_at, updated_at
        FROM services
        WHERE name = ?1
        ",
        params![service_name],
        |row| {
            Ok(ServiceRecord {
                id: row.get(0)?,
                name: row.get(1)?,
                status: row.get(2)?,
                app_path: row.get(3)?,
                manifest_json: row.get(4)?,
                plan_json: row.get(5)?,
                pid: row.get(6)?,
                port: row.get::<_, Option<u16>>(7)?,
                health_path: row.get(8)?,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

pub fn list_services(conn: &Connection) -> Result<Vec<ServiceRecord>> {
    let mut stmt = conn.prepare(
        "
        SELECT id, name, status, app_path, manifest_json, plan_json, pid, port, health_path, created_at, updated_at
        FROM services
        ORDER BY name
        ",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(ServiceRecord {
            id: row.get(0)?,
            name: row.get(1)?,
            status: row.get(2)?,
            app_path: row.get(3)?,
            manifest_json: row.get(4)?,
            plan_json: row.get(5)?,
            pid: row.get(6)?,
            port: row.get::<_, Option<u16>>(7)?,
            health_path: row.get(8)?,
            created_at: row.get(9)?,
            updated_at: row.get(10)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn update_service_status(
    conn: &Connection,
    service_name: &str,
    status: &str,
    pid: Option<i32>,
) -> Result<()> {
    conn.execute(
        "UPDATE services SET status = ?2, pid = ?3, updated_at = ?4 WHERE name = ?1",
        params![service_name, status, pid, now_string()],
    )?;
    Ok(())
}

pub fn delete_service(conn: &Connection, service_name: &str) -> Result<()> {
    for sql in [
        "DELETE FROM deployments WHERE service_name = ?1",
        "DELETE FROM routes WHERE service_name = ?1",
        "DELETE FROM secrets WHERE service_name = ?1",
        "DELETE FROM sandboxes WHERE service_name = ?1",
        "DELETE FROM services WHERE name = ?1",
    ] {
        conn.execute(sql, params![service_name])?;
    }
    Ok(())
}

pub fn put_route(
    conn: &Connection,
    service_name: &str,
    domain: &str,
    port: Option<u16>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO routes(service_name, domain, port, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![service_name, domain, port.map(i64::from), now_string()],
    )?;
    Ok(())
}

pub fn list_routes(conn: &Connection, service_name: Option<&str>) -> Result<Vec<RouteRecord>> {
    let sql = if service_name.is_some() {
        "SELECT id, service_name, domain, port, created_at FROM routes WHERE service_name = ?1 ORDER BY domain"
    } else {
        "SELECT id, service_name, domain, port, created_at FROM routes ORDER BY domain"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = if let Some(service_name) = service_name {
        stmt.query_map(params![service_name], |row| {
            Ok(RouteRecord {
                id: row.get(0)?,
                service_name: row.get(1)?,
                domain: row.get(2)?,
                port: row.get::<_, Option<u16>>(3)?,
                created_at: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        stmt.query_map([], |row| {
            Ok(RouteRecord {
                id: row.get(0)?,
                service_name: row.get(1)?,
                domain: row.get(2)?,
                port: row.get::<_, Option<u16>>(3)?,
                created_at: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    Ok(rows)
}

pub fn ensure_volume(
    conn: &Connection,
    paths: &StatePaths,
    service_name: Option<&str>,
    volume_name: &str,
) -> Result<VolumeRecord> {
    let host_path = paths.volumes_dir.join(volume_name);
    fs::create_dir_all(&host_path)?;
    conn.execute(
        "
        INSERT INTO volumes(service_name, name, host_path, created_at)
        VALUES (?1, ?2, ?3, ?4)
        ON CONFLICT(name) DO UPDATE SET
            service_name = COALESCE(excluded.service_name, service_name),
            host_path = excluded.host_path
        ",
        params![
            service_name,
            volume_name,
            host_path.to_string_lossy(),
            now_string()
        ],
    )?;
    Ok(volume_by_name(conn, volume_name)?.expect("volume just created"))
}

pub fn volume_by_name(conn: &Connection, volume_name: &str) -> Result<Option<VolumeRecord>> {
    conn.query_row(
        "SELECT id, service_name, name, host_path, created_at FROM volumes WHERE name = ?1",
        params![volume_name],
        |row| {
            Ok(VolumeRecord {
                id: row.get(0)?,
                service_name: row.get(1)?,
                name: row.get(2)?,
                host_path: row.get(3)?,
                created_at: row.get(4)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

pub fn list_volumes(conn: &Connection) -> Result<Vec<VolumeRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, service_name, name, host_path, created_at FROM volumes ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(VolumeRecord {
            id: row.get(0)?,
            service_name: row.get(1)?,
            name: row.get(2)?,
            host_path: row.get(3)?,
            created_at: row.get(4)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn set_secret(conn: &Connection, service_name: &str, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "
        INSERT INTO secrets(service_name, key, value, created_at)
        VALUES (?1, ?2, ?3, ?4)
        ON CONFLICT(service_name, key) DO UPDATE SET value = excluded.value
        ",
        params![service_name, key, value, now_string()],
    )?;
    Ok(())
}

pub fn list_secrets(conn: &Connection, service_name: Option<&str>) -> Result<Vec<SecretRecord>> {
    let sql = if service_name.is_some() {
        "SELECT id, service_name, key, value, created_at FROM secrets WHERE service_name = ?1 ORDER BY key"
    } else {
        "SELECT id, service_name, key, value, created_at FROM secrets ORDER BY service_name, key"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = if let Some(service_name) = service_name {
        stmt.query_map(params![service_name], |row| {
            Ok(SecretRecord {
                id: row.get(0)?,
                service_name: row.get(1)?,
                key: row.get(2)?,
                value: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        stmt.query_map([], |row| {
            Ok(SecretRecord {
                id: row.get(0)?,
                service_name: row.get(1)?,
                key: row.get(2)?,
                value: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    Ok(rows)
}

pub fn secret_value(conn: &Connection, service_name: &str, key: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM secrets WHERE service_name = ?1 AND key = ?2",
        params![service_name, key],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

pub fn insert_snapshot(
    conn: &Connection,
    service_name: &str,
    snapshot_name: &str,
    snapshot_path: &Path,
) -> Result<()> {
    conn.execute(
        "INSERT INTO snapshots(service_name, snapshot_name, snapshot_path, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![service_name, snapshot_name, snapshot_path.to_string_lossy(), now_string()],
    )?;
    Ok(())
}

pub fn snapshot_by_name(conn: &Connection, snapshot_name: &str) -> Result<Option<SnapshotRecord>> {
    conn.query_row(
        "SELECT id, service_name, snapshot_name, snapshot_path, created_at FROM snapshots WHERE snapshot_name = ?1",
        params![snapshot_name],
        |row| {
            Ok(SnapshotRecord {
                id: row.get(0)?,
                service_name: row.get(1)?,
                snapshot_name: row.get(2)?,
                snapshot_path: row.get(3)?,
                created_at: row.get(4)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

pub fn emit_event(
    conn: &Connection,
    service_name: Option<&str>,
    kind: &str,
    details: Value,
) -> Result<()> {
    conn.execute(
        "INSERT INTO events(service_name, kind, details_json, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![
            service_name,
            kind,
            serde_json::to_string(&details)?,
            now_string()
        ],
    )?;
    Ok(())
}

pub fn list_events(conn: &Connection, service_name: Option<&str>) -> Result<Vec<EventRecord>> {
    let sql = if service_name.is_some() {
        "SELECT id, service_name, kind, details_json, created_at FROM events WHERE service_name = ?1 ORDER BY id DESC LIMIT 100"
    } else {
        "SELECT id, service_name, kind, details_json, created_at FROM events ORDER BY id DESC LIMIT 100"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = if let Some(service_name) = service_name {
        stmt.query_map(params![service_name], |row| {
            Ok(EventRecord {
                id: row.get(0)?,
                service_name: row.get(1)?,
                kind: row.get(2)?,
                details_json: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        stmt.query_map([], |row| {
            Ok(EventRecord {
                id: row.get(0)?,
                service_name: row.get(1)?,
                kind: row.get(2)?,
                details_json: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    Ok(rows)
}

pub struct SandboxUpsert<'a> {
    pub service_name: &'a str,
    pub sandbox_id: &'a str,
    pub hostname: &'a str,
    pub ip_address: Option<&'a str>,
    pub runtime_kind: &'a str,
    pub isolation_mode: &'a str,
    pub status: &'a str,
    pub pid: Option<i32>,
    pub cgroup_path: Option<&'a str>,
}

pub fn upsert_sandbox(conn: &Connection, sandbox: SandboxUpsert<'_>) -> Result<()> {
    let now = now_string();
    conn.execute(
        "
        INSERT INTO sandboxes(
            service_name, sandbox_id, hostname, ip_address, runtime_kind, isolation_mode,
            status, pid, cgroup_path, created_at, updated_at
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
        ON CONFLICT(service_name) DO UPDATE SET
            sandbox_id = excluded.sandbox_id,
            hostname = excluded.hostname,
            ip_address = excluded.ip_address,
            runtime_kind = excluded.runtime_kind,
            isolation_mode = excluded.isolation_mode,
            status = excluded.status,
            pid = excluded.pid,
            cgroup_path = excluded.cgroup_path,
            updated_at = excluded.updated_at
        ",
        params![
            sandbox.service_name,
            sandbox.sandbox_id,
            sandbox.hostname,
            sandbox.ip_address,
            sandbox.runtime_kind,
            sandbox.isolation_mode,
            sandbox.status,
            sandbox.pid,
            sandbox.cgroup_path,
            now,
        ],
    )?;
    Ok(())
}

pub fn sandbox_by_service(conn: &Connection, service_name: &str) -> Result<Option<SandboxRecord>> {
    conn.query_row(
        "
        SELECT id, service_name, sandbox_id, hostname, ip_address, runtime_kind, isolation_mode,
               status, pid, cgroup_path, created_at, updated_at
        FROM sandboxes
        WHERE service_name = ?1
        ",
        params![service_name],
        |row| {
            Ok(SandboxRecord {
                id: row.get(0)?,
                service_name: row.get(1)?,
                sandbox_id: row.get(2)?,
                hostname: row.get(3)?,
                ip_address: row.get(4)?,
                runtime_kind: row.get(5)?,
                isolation_mode: row.get(6)?,
                status: row.get(7)?,
                pid: row.get(8)?,
                cgroup_path: row.get(9)?,
                created_at: row.get(10)?,
                updated_at: row.get(11)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

pub fn list_sandboxes(conn: &Connection) -> Result<Vec<SandboxRecord>> {
    let mut stmt = conn.prepare(
        "
        SELECT id, service_name, sandbox_id, hostname, ip_address, runtime_kind, isolation_mode,
               status, pid, cgroup_path, created_at, updated_at
        FROM sandboxes
        ORDER BY service_name
        ",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(SandboxRecord {
            id: row.get(0)?,
            service_name: row.get(1)?,
            sandbox_id: row.get(2)?,
            hostname: row.get(3)?,
            ip_address: row.get(4)?,
            runtime_kind: row.get(5)?,
            isolation_mode: row.get(6)?,
            status: row.get(7)?,
            pid: row.get(8)?,
            cgroup_path: row.get(9)?,
            created_at: row.get(10)?,
            updated_at: row.get(11)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn delete_sandbox(conn: &Connection, service_name: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM sandboxes WHERE service_name = ?1",
        params![service_name],
    )?;
    Ok(())
}

use crate::app;
use crate::state::{self, StatePaths};
use serde_json::{Value, json};
use std::env;
use std::ffi::c_char;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static LAST_ERROR: OnceLock<Mutex<Vec<u8>>> = OnceLock::new();
const HOST_INPUT: &str = "/tmp/megaserver.fzy.host.input.json";
const HOST_OUTPUT: &str = "/tmp/megaserver.fzy.host.output.json";

pub fn link_host_abi() {
    let _ = megaserver_host_dispatch as extern "C" fn() -> i32;
    let _ = megaserver_host_last_error_message as extern "C" fn() -> *const c_char;
}

#[used]
static HOST_DISPATCH_SYMBOL: extern "C" fn() -> i32 = megaserver_host_dispatch;

#[used]
static HOST_DISPATCH_ALIAS_SYMBOL: extern "C" fn() -> i32 = megaserver_host_dispatch_alias;

#[used]
static HOST_ERROR_SYMBOL: extern "C" fn() -> *const c_char = megaserver_host_last_error_message;

fn last_error_slot() -> &'static Mutex<Vec<u8>> {
    LAST_ERROR.get_or_init(|| Mutex::new(b"megaserver host abi idle\0".to_vec()))
}

fn set_last_error(message: &str) {
    let mut bytes = message.as_bytes().to_vec();
    bytes.retain(|byte| *byte != 0);
    bytes.push(0);
    if let Ok(mut slot) = last_error_slot().lock() {
        *slot = bytes;
    }
}

fn control_output_path() -> PathBuf {
    env::var_os("MEGASERVER_FZY_CONTROL_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/tmp/megaserver.fzy.control.output.json"))
}

fn resolve_paths(request: &Value) -> anyhow::Result<StatePaths> {
    let home = request
        .get("home")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let paths = StatePaths::resolve(home)?;
    state::init(&paths)?;
    Ok(paths)
}

fn action_string(request: &Value) -> anyhow::Result<&str> {
    request
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing action"))
}

fn dispatch_value(request: &Value) -> anyhow::Result<Value> {
    let paths = resolve_paths(request)?;
    match action_string(request)? {
        "deploy" => {
            let app_path = request
                .get("app_path")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing app_path"))?;
            let service = app::deploy_and_start_only(&paths, Path::new(app_path))?;
            app::inspect_value(&paths, &service)
        }
        "start" => {
            let service = request
                .get("service")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing service"))?;
            app::start_only(&paths, service)?;
            Ok(app::inspect_value(&paths, service)?)
        }
        "stop" => {
            let service = request
                .get("service")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing service"))?;
            app::stop_only(&paths, service)?;
            Ok(json!({"status":"ok","service":service,"action":"stop"}))
        }
        "restart" => {
            let service = request
                .get("service")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing service"))?;
            app::stop_only(&paths, service)?;
            app::start_only(&paths, service)?;
            Ok(app::inspect_value(&paths, service)?)
        }
        "destroy" => {
            let service = request
                .get("service")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing service"))?;
            app::destroy_only(&paths, service)?;
            Ok(json!({"status":"ok","service":service,"action":"destroy"}))
        }
        "services" => {
            let conn = state::open(&paths)?;
            Ok(json!(state::list_services(&conn)?))
        }
        "logs" => {
            let service = request
                .get("service")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing service"))?;
            let lines = request
                .get("lines")
                .and_then(Value::as_u64)
                .and_then(|v| usize::try_from(v).ok())
                .unwrap_or(50);
            app::logs_value(&paths, service, lines)
        }
        "route_add" => {
            let service = request
                .get("service")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing service"))?;
            let domain = request
                .get("domain")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing domain"))?;
            let port = request
                .get("port")
                .and_then(Value::as_u64)
                .and_then(|v| u16::try_from(v).ok());
            app::expose_value(&paths, service, domain, port)
        }
        "route_list" => {
            let conn = state::open(&paths)?;
            let service = request.get("service").and_then(Value::as_str);
            Ok(json!(state::list_routes(&conn, service)?))
        }
        "route_sign" => {
            let service = request
                .get("service")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing service"))?;
            let domain = request
                .get("domain")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing domain"))?;
            let path = request
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing path"))?;
            let expires_in = request
                .get("expires_in")
                .and_then(Value::as_u64)
                .unwrap_or(300);
            let scheme = request
                .get("scheme")
                .and_then(Value::as_str)
                .unwrap_or("https");
            app::signed_link_value(&paths, service, domain, path, expires_in, scheme)
        }
        "volume_create" => {
            let name = request
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing name"))?;
            let service = request.get("service").and_then(Value::as_str);
            Ok(json!(app::create_volume_value(&paths, name, service)?))
        }
        "volume_list" => {
            let conn = state::open(&paths)?;
            Ok(json!(state::list_volumes(&conn)?))
        }
        "secret_set" => {
            let service = request
                .get("service")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing service"))?;
            let key = request
                .get("key")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing key"))?;
            let value = request
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing value"))?;
            app::set_secret_only(&paths, service, key, value)?;
            Ok(json!({"status":"ok","service":service,"key":key}))
        }
        "secret_list" => {
            let conn = state::open(&paths)?;
            let service = request.get("service").and_then(Value::as_str);
            let redacted = state::list_secrets(&conn, service)?
                .into_iter()
                .map(|secret| json!({"service": secret.service_name, "key": secret.key, "value": "***"}))
                .collect::<Vec<_>>();
            Ok(json!(redacted))
        }
        "snapshot" => {
            let service = request
                .get("service")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing service"))?;
            app::snapshot_value(&paths, service)
        }
        "rollback" => {
            let service = request
                .get("service")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing service"))?;
            let snapshot = request
                .get("snapshot")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing snapshot"))?;
            app::rollback_only(&paths, service, snapshot)?;
            Ok(app::inspect_value(&paths, service)?)
        }
        "inspect" => {
            let service = request
                .get("service")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing service"))?;
            app::inspect_value(&paths, service)
        }
        "events" => {
            let conn = state::open(&paths)?;
            let service = request.get("service").and_then(Value::as_str);
            Ok(json!(state::list_events(&conn, service)?))
        }
        "health" => {
            let conn = state::open(&paths)?;
            let service_count = state::list_services(&conn)?.len();
            let sandbox_count = state::list_sandboxes(&conn)?.len();
            let route_count = state::list_routes(&conn, None)?.len();
            Ok(json!({
                "status":"ok",
                "home":paths.home,
                "service_count": service_count,
                "sandbox_count": sandbox_count,
                "route_count": route_count,
                "route_runtime_count": route_count,
                "ingress":"running"
            }))
        }
        "shell" => {
            let service = request
                .get("service")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing service"))?;
            let command = request
                .get("command")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            app::shell_only(&paths, service, &command)?;
            Ok(json!({"status":"ok","service":service,"action":"shell"}))
        }
        other => Err(anyhow::anyhow!("unknown action `{other}`")),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn megaserver_host_last_error_message() -> *const c_char {
    if let Ok(slot) = last_error_slot().lock() {
        slot.as_ptr().cast()
    } else {
        c"megaserver host abi poisoned".as_ptr()
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn megaserver_host_dispatch() -> i32 {
    let request_text = match std::fs::read_to_string(HOST_INPUT) {
        Ok(text) => text,
        Err(err) => {
            set_last_error(&format!("read control input failed: {err}"));
            let _ = std::fs::write(
                HOST_OUTPUT,
                json!({"status":"error","message":format!("read control input failed: {err}"),"control_plane":"rust-host"}).to_string(),
            );
            return 21;
        }
    };

    let request = match serde_json::from_str::<Value>(&request_text) {
        Ok(value) => value,
        Err(err) => {
            set_last_error(&format!("invalid control request json: {err}"));
            let _ = std::fs::write(
                HOST_OUTPUT,
                json!({"status":"error","message":format!("invalid control request json: {err}"),"control_plane":"rust-host"}).to_string(),
            );
            return 22;
        }
    };

    match dispatch_value(&request) {
        Ok(value) => {
            if let Err(err) = std::fs::write(HOST_OUTPUT, value.to_string()) {
                set_last_error(&format!("write control output failed: {err}"));
                return 5;
            }
            0
        }
        Err(err) => {
            set_last_error(&err.to_string());
            let _ = std::fs::write(
                HOST_OUTPUT,
                json!({"status":"error","message":err.to_string(),"control_plane":"rust-host"})
                    .to_string(),
            );
            1
        }
    }
}

#[unsafe(export_name = "api_ffi_megaserver_host_dispatch")]
pub extern "C" fn megaserver_host_dispatch_alias() -> i32 {
    megaserver_host_dispatch()
}

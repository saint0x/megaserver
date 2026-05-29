use crate::app;
use crate::state::{self, StatePaths};
use serde_json::{Map, Value, json};
use std::env;
use std::ffi::c_char;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use url::form_urlencoded;

static LAST_ERROR: OnceLock<Mutex<Vec<u8>>> = OnceLock::new();

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

fn host_input_path() -> PathBuf {
    sibling_of_control_path(
        "MEGASERVER_FZY_CONTROL_INPUT",
        "megaserver.fzy.control.input.json",
        "megaserver.fzy.host.input.json",
    )
}

fn host_output_path() -> PathBuf {
    sibling_of_control_path(
        "MEGASERVER_FZY_CONTROL_OUTPUT",
        "megaserver.fzy.control.output.json",
        "megaserver.fzy.host.output.json",
    )
}

fn sibling_of_control_path(
    env_key: &str,
    default_control_name: &str,
    sibling_name: &str,
) -> PathBuf {
    let control_path = env::var_os(env_key)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/var/tmp/{default_control_name}")));
    control_path
        .parent()
        .map(|parent| parent.join(sibling_name))
        .unwrap_or_else(|| PathBuf::from(format!("/var/tmp/{sibling_name}")))
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

fn json_object_field<'a>(body: &'a Map<String, Value>, key: &str) -> anyhow::Result<&'a Value> {
    body.get(key)
        .ok_or_else(|| anyhow::anyhow!("missing {key}"))
}

fn json_string_field<'a>(body: &'a Map<String, Value>, key: &str) -> anyhow::Result<&'a str> {
    json_object_field(body, key)?
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing {key}"))
}

fn parse_request_body(request: &Value) -> anyhow::Result<Map<String, Value>> {
    let Some(body) = request.get("body") else {
        return Ok(Map::new());
    };
    let body = body
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("invalid body"))?
        .trim();
    if body.is_empty() {
        return Ok(Map::new());
    }
    serde_json::from_str::<Map<String, Value>>(body)
        .map_err(|err| anyhow::anyhow!("invalid JSON body: {err}"))
}

fn parse_query_params(path: &str) -> Map<String, Value> {
    let mut query = Map::new();
    let Some((_, raw_query)) = path.split_once('?') else {
        return query;
    };
    for (key, value) in form_urlencoded::parse(raw_query.as_bytes()) {
        query.insert(key.into_owned(), Value::String(value.into_owned()));
    }
    query
}

fn service_name_from_path<'a>(path: &'a str, suffix: &str) -> Option<&'a str> {
    let (route_path, _) = path.split_once('?').unwrap_or((path, ""));
    let prefix = "/v1/services/";
    let trimmed = route_path.strip_prefix(prefix)?;
    let service = trimmed.strip_suffix(suffix)?;
    if service.is_empty() {
        None
    } else {
        Some(service)
    }
}

fn rollback_names_from_path(path: &str) -> Option<(&str, &str)> {
    let (route_path, _) = path.split_once('?').unwrap_or((path, ""));
    let trimmed = route_path.strip_prefix("/v1/services/")?;
    let (service, snapshot) = trimmed.split_once("/rollback/")?;
    if service.is_empty() || snapshot.is_empty() {
        None
    } else {
        Some((service, snapshot))
    }
}

fn http_response(status: u16, body: Value) -> Value {
    let body_text = if body.is_string() {
        body.as_str().unwrap_or_default().to_string()
    } else {
        body.to_string()
    };
    json!({
        "http_status": status,
        "content_type": "application/json; charset=utf-8",
        "body": body_text
    })
}

fn dispatch_http_value(request: &Value) -> anyhow::Result<Value> {
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing method"))?;
    let path = request
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing path"))?;
    let body = parse_request_body(request)?;
    let query = parse_query_params(path);

    let action_request = match (method, path.split('?').next().unwrap_or(path)) {
        ("GET", "/v1/health") => json!({"home": request["home"], "action": "health"}),
        ("GET", "/v1/services") => json!({"home": request["home"], "action": "services"}),
        ("POST", "/v1/services/deploy") => json!({
            "home": request["home"],
            "action": "deploy",
            "app_path": json_string_field(&body, "app_path")?,
        }),
        ("GET", "/v1/routes") => {
            let mut payload = json!({"home": request["home"], "action": "route_list"});
            if let Some(service) = query.get("service").and_then(Value::as_str) {
                payload["service"] = Value::String(service.to_string());
            }
            payload
        }
        ("POST", "/v1/routes") => json!({
            "home": request["home"],
            "action": "route_add",
            "service": json_string_field(&body, "service")?,
            "domain": json_string_field(&body, "domain")?,
            "port": body.get("port").cloned().unwrap_or(Value::Null),
        }),
        ("POST", "/v1/routes/signed-link") => json!({
            "home": request["home"],
            "action": "route_sign",
            "service": json_string_field(&body, "service")?,
            "domain": json_string_field(&body, "domain")?,
            "path": json_string_field(&body, "path")?,
            "expires_in": body.get("expires_in").cloned().unwrap_or(Value::from(300_u64)),
            "scheme": body.get("scheme").cloned().unwrap_or(Value::String("https".to_string())),
        }),
        ("GET", "/v1/volumes") => json!({"home": request["home"], "action": "volume_list"}),
        ("POST", "/v1/volumes") => {
            let mut payload = json!({
                "home": request["home"],
                "action": "volume_create",
                "name": json_string_field(&body, "name")?,
            });
            if let Some(service) = body.get("service").and_then(Value::as_str) {
                payload["service"] = Value::String(service.to_string());
            }
            payload
        }
        ("GET", "/v1/secrets") => {
            let mut payload = json!({"home": request["home"], "action": "secret_list"});
            if let Some(service) = query.get("service").and_then(Value::as_str) {
                payload["service"] = Value::String(service.to_string());
            }
            payload
        }
        ("POST", "/v1/secrets") => json!({
            "home": request["home"],
            "action": "secret_set",
            "service": json_string_field(&body, "service")?,
            "key": json_string_field(&body, "key")?,
            "value": json_string_field(&body, "value")?,
        }),
        ("GET", "/v1/events") => {
            let mut payload = json!({"home": request["home"], "action": "events"});
            if let Some(service) = query.get("service").and_then(Value::as_str) {
                payload["service"] = Value::String(service.to_string());
            }
            payload
        }
        _ => {
            if method == "POST" {
                if let Some(service) = service_name_from_path(path, "/start") {
                    json!({"home": request["home"], "action": "start", "service": service})
                } else if let Some(service) = service_name_from_path(path, "/stop") {
                    json!({"home": request["home"], "action": "stop", "service": service})
                } else if let Some(service) = service_name_from_path(path, "/restart") {
                    json!({"home": request["home"], "action": "restart", "service": service})
                } else if let Some(service) = service_name_from_path(path, "/destroy") {
                    json!({"home": request["home"], "action": "destroy", "service": service})
                } else if let Some(service) = service_name_from_path(path, "/shell") {
                    json!({
                        "home": request["home"],
                        "action": "shell",
                        "service": service,
                        "command": body.get("command").cloned().unwrap_or(Value::Array(Vec::new())),
                    })
                } else if let Some(service) = service_name_from_path(path, "/snapshot") {
                    json!({"home": request["home"], "action": "snapshot", "service": service})
                } else if let Some((service, snapshot)) = rollback_names_from_path(path) {
                    json!({
                        "home": request["home"],
                        "action": "rollback",
                        "service": service,
                        "snapshot": snapshot,
                    })
                } else {
                    return Ok(http_response(
                        404,
                        json!({"status":"error","message": format!("unknown route `{method} {path}`")}),
                    ));
                }
            } else if method == "GET" {
                if let Some(service) = service_name_from_path(path, "/inspect") {
                    json!({"home": request["home"], "action": "inspect", "service": service})
                } else if let Some(service) = service_name_from_path(path, "/logs") {
                    let mut payload =
                        json!({"home": request["home"], "action": "logs", "service": service});
                    if let Some(lines) = query
                        .get("lines")
                        .and_then(Value::as_str)
                        .and_then(|raw| raw.parse::<u64>().ok())
                    {
                        payload["lines"] = Value::from(lines);
                    }
                    payload
                } else {
                    return Ok(http_response(
                        404,
                        json!({"status":"error","message": format!("unknown route `{method} {path}`")}),
                    ));
                }
            } else {
                return Ok(http_response(
                    405,
                    json!({"status":"error","message": format!("unsupported method `{method}`")}),
                ));
            }
        }
    };

    match dispatch_value(&action_request) {
        Ok(value) => Ok(http_response(200, value)),
        Err(err) => Ok(http_response(
            400,
            json!({"status":"error","message": err.to_string(),"control_plane":"rust-host"}),
        )),
    }
}

fn service_target_host(sandbox: Option<&crate::model::SandboxRecord>) -> &str {
    match sandbox {
        Some(sandbox) if sandbox.runtime_kind == "linux-namespace" => {
            sandbox.ip_address.as_deref().unwrap_or("127.0.0.1")
        }
        _ => "127.0.0.1",
    }
}

fn ingress_resolve_value(paths: &StatePaths, request: &Value) -> anyhow::Result<Value> {
    let host = request
        .get("host")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing host"))?;
    let path = request
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing path"))?;
    let query = request
        .get("query")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());

    let conn = state::open(paths)?;
    let route = state::list_routes(&conn, None)?
        .into_iter()
        .find(|route| route.domain == host)
        .ok_or_else(|| anyhow::anyhow!("route not found for host `{host}`"))?;
    let service = state::service_by_name(&conn, &route.service_name)?
        .ok_or_else(|| anyhow::anyhow!("unknown service `{}`", route.service_name))?;
    if service.status != "healthy" && service.status != "degraded" {
        anyhow::bail!("service unavailable");
    }
    let sandbox = state::sandbox_by_service(&conn, &route.service_name)?;
    let upstream_host = service_target_host(sandbox.as_ref()).to_string();
    let upstream_port = route
        .port
        .ok_or_else(|| anyhow::anyhow!("missing route port"))?;
    let upstream_path = if path == "/_megaserver/signed" {
        let secret = state::secret_value(&conn, &route.service_name, "MEGASERVER_SIGNING_KEY")?
            .ok_or_else(|| anyhow::anyhow!("signed links are not enabled for this service"))?;
        crate::ingress::resolve_signed_target(&secret, host, &route.service_name, query)?
    } else if let Some(query) = query {
        format!("{path}?{query}")
    } else {
        path.to_string()
    };
    Ok(json!({
        "status": "ok",
        "service": route.service_name,
        "service_status": service.status,
        "upstream_host": upstream_host,
        "upstream_port": upstream_port,
        "upstream_path": upstream_path
    }))
}

fn dispatch_value(request: &Value) -> anyhow::Result<Value> {
    if request.get("method").is_some() {
        return dispatch_http_value(request);
    }
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
        "ingress_resolve" => ingress_resolve_value(&paths, request),
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
    let request_text = match std::fs::read_to_string(host_input_path()) {
        Ok(text) => text,
        Err(err) => {
            set_last_error(&format!("read control input failed: {err}"));
            let _ = std::fs::write(
                host_output_path(),
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
                host_output_path(),
                json!({"status":"error","message":format!("invalid control request json: {err}"),"control_plane":"rust-host"}).to_string(),
            );
            return 22;
        }
    };

    match dispatch_value(&request) {
        Ok(value) => {
            if let Err(err) = std::fs::write(host_output_path(), value.to_string()) {
                set_last_error(&format!("write control output failed: {err}"));
                return 5;
            }
            0
        }
        Err(err) => {
            set_last_error(&err.to_string());
            let _ = std::fs::write(
                host_output_path(),
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

use anyhow::{Context, Result, anyhow, bail};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};
use url::form_urlencoded;

type HmacSha256 = Hmac<Sha256>;

pub fn signed_link(
    scheme: &str,
    domain: &str,
    service: &str,
    target: &str,
    expires_in: u64,
    secret: &str,
) -> Result<serde_json::Value> {
    if !(scheme == "http" || scheme == "https") {
        bail!("scheme must be `http` or `https`");
    }
    if !target.starts_with('/') {
        bail!("signed link target must start with `/`");
    }

    let expires_at = now_epoch_seconds() + expires_in;
    let signature = sign(secret, domain, service, target, expires_at)?;

    let mut serializer = form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("target", target);
    serializer.append_pair("exp", &expires_at.to_string());
    serializer.append_pair("sig", &signature);
    let ingress_path = format!("/_megaserver/signed?{}", serializer.finish());

    Ok(serde_json::json!({
        "status": "ok",
        "service": service,
        "domain": domain,
        "scheme": scheme,
        "target": target,
        "expires_at": expires_at,
        "ingress_path": ingress_path,
        "url": format!("{scheme}://{domain}{ingress_path}")
    }))
}

pub fn resolve_signed_target(
    secret: &str,
    domain: &str,
    service: &str,
    query: Option<&str>,
) -> Result<String> {
    let query = query.ok_or_else(|| anyhow!("missing signed-link query"))?;
    let params = form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect::<std::collections::HashMap<String, String>>();
    let target = params
        .get("target")
        .cloned()
        .ok_or_else(|| anyhow!("missing signed-link target"))?;
    let expires_at = params
        .get("exp")
        .ok_or_else(|| anyhow!("missing signed-link expiry"))?
        .parse::<u64>()
        .context("invalid signed-link expiry")?;
    let signature = params
        .get("sig")
        .cloned()
        .ok_or_else(|| anyhow!("missing signed-link signature"))?;
    if now_epoch_seconds() > expires_at {
        bail!("signed link has expired");
    }
    let expected = sign(secret, domain, service, &target, expires_at)?;
    if expected != signature {
        bail!("signed link signature mismatch");
    }
    Ok(target)
}

fn sign(
    secret: &str,
    domain: &str,
    service: &str,
    target: &str,
    expires_at: u64,
) -> Result<String> {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).context("invalid signing key material")?;
    mac.update(signature_payload(domain, service, target, expires_at).as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

fn signature_payload(domain: &str, service: &str, target: &str, expires_at: u64) -> String {
    format!("{domain}\n{service}\n{target}\n{expires_at}")
}

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::{resolve_signed_target, signed_link};

    #[test]
    fn signed_links_round_trip() {
        let value = signed_link(
            "https",
            "hello.local",
            "hello-service",
            "/private/report?token=1",
            300,
            "secret",
        )
        .unwrap();
        let path = value["ingress_path"].as_str().unwrap();
        let query = path.split_once('?').unwrap().1;
        let target =
            resolve_signed_target("secret", "hello.local", "hello-service", Some(query)).unwrap();
        assert_eq!(target, "/private/report?token=1");
    }
}

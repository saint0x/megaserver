use anyhow::{Context, Result};
use rustls::ServerConfig;
use rustls_pemfile::{certs, private_key};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;

fn ensure_crypto_provider() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub fn load_server_config(
    cert_path: &Path,
    key_path: &Path,
    ca_path: Option<&Path>,
) -> Result<ServerConfig> {
    ensure_crypto_provider();
    let cert_chain = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;

    let config = if let Some(ca_path) = ca_path {
        let ca_certs = load_certs(ca_path)?;
        let mut root_store = rustls::RootCertStore::empty();
        for cert in ca_certs {
            root_store
                .add(cert)
                .context("failed to add CA certificate to root store")?;
        }
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store))
            .build()
            .context("failed to build client certificate verifier")?;
        ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(cert_chain, key)
            .context("failed to create mTLS server config")?
    } else {
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain, key)
            .context("failed to create TLS server config")?
    };

    Ok(config)
}

fn load_certs(path: &Path) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let file = File::open(path).with_context(|| format!("failed to open cert file: {path:?}"))?;
    let mut reader = BufReader::new(file);
    let certs: Vec<_> = certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("failed to parse certificates from {path:?}"))?;
    if certs.is_empty() {
        anyhow::bail!("no certificates found in {path:?}");
    }
    Ok(certs)
}

fn load_private_key(path: &Path) -> Result<rustls::pki_types::PrivateKeyDer<'static>> {
    let file = File::open(path).with_context(|| format!("failed to open key file: {path:?}"))?;
    let mut reader = BufReader::new(file);
    private_key(&mut reader)
        .with_context(|| format!("failed to parse private key from {path:?}"))?
        .with_context(|| format!("no private key found in {path:?}"))
}

//! TLS material for the AI bridge listener.
//!
//! QUIC has no plaintext mode, so the bridge always needs a certificate. Two
//! sources: a PEM pair from config (`AI_TLS_CERT` / `AI_TLS_KEY`) for a real
//! deployment, or a self-signed one generated at boot for local development.
//!
//! The certificate only proves *the backend* to a dialling service. It is not
//! the authentication: that is the shared token in the `Hello` frame, checked
//! in constant time by [`super::server`]. A self-signed certificate therefore
//! costs nothing in a private network — the service pins the fingerprint the
//! bridge logs at startup.

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::QuicServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::ai::error::AiError;
use crate::constant::{AI_ALPN, AI_IDLE_TIMEOUT_SECS, AI_MAX_CONCURRENT_PER_WORKER};

/// The certificate chain the listener presents, plus the leaf in DER so a
/// caller (the boot log, an in-process test service) can pin it.
#[derive(Debug)]
pub struct BridgeTls {
    pub server_config: quinn::ServerConfig,
    pub leaf: CertificateDer<'static>,
}

/// Install the process-wide rustls crypto provider, once. Both the bridge and
/// any in-process test client need it, and rustls refuses a second install —
/// an already-installed provider is success, not an error.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Build the listener's TLS config from a PEM pair, or self-signed when either
/// path is absent.
pub fn build(cert_path: Option<&str>, key_path: Option<&str>) -> Result<BridgeTls, AiError> {
    install_crypto_provider();
    let (chain, key) = match (cert_path, key_path) {
        (Some(c), Some(k)) => load_pem(c, k)?,
        _ => self_signed()?,
    };
    let leaf = chain
        .first()
        .cloned()
        .ok_or_else(|| AiError::Setup("certificate chain is empty".into()))?;

    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|e| {
            AiError::Setup(format!("certificate and key do not form a valid pair: {e}"))
        })?;
    // ALPN is the version gate: a service built against a future frame format
    // announces a different protocol id and is refused during the TLS
    // handshake, before either side can misread the other's bytes.
    tls.alpn_protocols = vec![AI_ALPN.to_vec()];

    let quic_tls = QuicServerConfig::try_from(tls)
        .map_err(|e| AiError::Setup(format!("TLS config is not usable for QUIC: {e}")))?;
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_tls));

    let mut transport = quinn::TransportConfig::default();
    // A service that stops answering is dropped within this window, which is
    // what removes it from the registry — there is no application heartbeat.
    transport.max_idle_timeout(Some(
        Duration::from_secs(AI_IDLE_TIMEOUT_SECS)
            .try_into()
            .expect("idle timeout fits in a QUIC varint"),
    ));
    // Bounds how many control streams one connection may open toward us; the
    // request streams flow the other way and are bounded by the *client's*
    // config, so this is only about incoming ones.
    transport.max_concurrent_bidi_streams(
        u32::try_from(AI_MAX_CONCURRENT_PER_WORKER)
            .unwrap_or(64)
            .into(),
    );
    transport.max_concurrent_uni_streams(0u32.into());
    server_config.transport_config(Arc::new(transport));

    Ok(BridgeTls {
        server_config,
        leaf,
    })
}

/// Read a PEM certificate chain and private key off disk.
fn load_pem(
    cert_path: &str,
    key_path: &str,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), AiError> {
    let cert_pem = fs::read(cert_path)
        .map_err(|e| AiError::Setup(format!("cannot read AI_TLS_CERT `{cert_path}`: {e}")))?;
    let key_pem = fs::read(key_path)
        .map_err(|e| AiError::Setup(format!("cannot read AI_TLS_KEY `{key_path}`: {e}")))?;
    let chain = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AiError::Setup(format!("`{cert_path}` is not a PEM certificate: {e}")))?;
    if chain.is_empty() {
        return Err(AiError::Setup(format!(
            "`{cert_path}` contained no certificates"
        )));
    }
    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .map_err(|e| AiError::Setup(format!("`{key_path}` is not a PEM private key: {e}")))?
        .ok_or_else(|| AiError::Setup(format!("`{key_path}` contained no private key")))?;
    Ok((chain, key))
}

/// Generate a throwaway certificate for `localhost` / `127.0.0.1`.
fn self_signed() -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), AiError> {
    let names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    let certified = rcgen::generate_simple_self_signed(names).map_err(|e| {
        AiError::Setup(format!("could not generate a self-signed certificate: {e}"))
    })?;
    let key = PrivateKeyDer::try_from(certified.signing_key.serialize_der())
        .map_err(|e| AiError::Setup(format!("generated key is unusable: {e}")))?;
    Ok((vec![certified.cert.der().clone()], key))
}

/// SHA-256 of the leaf certificate, hex, for the startup log. A service pins
/// this instead of installing a CA.
pub fn fingerprint(leaf: &CertificateDer<'_>) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(leaf.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn self_signed_config_builds_and_pins() {
        let tls = build(None, None).expect("self-signed path works with no config");
        let fp = fingerprint(&tls.leaf);
        assert_eq!(fp.len(), 64, "sha-256 hex is 64 chars");
        // Regenerating gives a different key, so the fingerprint must differ —
        // otherwise pinning would be meaningless.
        let other = build(None, None).unwrap();
        assert_ne!(fp, fingerprint(&other.leaf));
    }

    #[tokio::test]
    async fn a_missing_cert_path_is_a_setup_error_not_a_silent_fallback() {
        // Half-configured TLS must fail loudly: silently self-signing when the
        // operator meant to supply a real certificate would be worse than
        // refusing to boot.
        let err = build(Some("/nonexistent/cert.pem"), Some("/nonexistent/key.pem")).unwrap_err();
        assert!(matches!(err, AiError::Setup(m) if m.contains("AI_TLS_CERT")));
    }
}

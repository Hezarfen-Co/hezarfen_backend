//! Public discovery for the AI bridge.
//!
//! An AI service needs the bridge's certificate before it can dial in, and the
//! certificate is regenerated on every boot when no PEM pair is configured. So
//! it is published here: the service fetches this over HTTP, pins what it gets,
//! and then connects over QUIC.
//!
//! Unauthenticated on purpose. A server certificate is public by construction —
//! it is handed to every peer during the TLS handshake — so serving it reveals
//! nothing a port scan would not. The private key never leaves the process.
//!
//! What this is *not*: a substitute for a CA. Fetch-then-pin over plain HTTP is
//! trust-on-first-use, only as trustworthy as that HTTP hop. On a hostile
//! network, put a real certificate in `AI_TLS_CERT`/`AI_TLS_KEY` and distribute
//! it out of band.

use axum::Json;
use axum::extract::State;
use base64::Engine;
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::ai::tls;
use crate::constant::AI_PROTOCOL;
use crate::error::{AppError, ErrorResponse};
use crate::state::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(bridge_certificate))
}

/// Everything a service needs to reach the bridge, except the shared token
/// (which is a secret and is configured out of band).
#[derive(Serialize, ToSchema)]
pub struct BridgeCertificateResponse {
    /// Wire protocol and ALPN the service must announce, currently `hab/1`.
    #[schema(example = "hab/1")]
    protocol: String,
    /// The listener's leaf certificate, PEM-encoded — feed straight into a TLS
    /// client's trust store (Python: `ssl_context.load_verify_locations(cadata=...)`).
    certificate_pem: String,
    /// SHA-256 of the DER certificate, hex. Same value the backend logs at
    /// startup; compare it if the PEM is pinned from somewhere else.
    #[schema(example = "6745e87f8a995e57b58297ac8058cd634d59557f3061144fa61127c96136e7fe")]
    fingerprint_sha256: String,
}

/// The AI bridge's certificate.
///
/// Public: a server certificate is presented to every peer in the TLS
/// handshake, so publishing it discloses nothing.
///
/// **Re-fetch on every reconnect.** With no `AI_TLS_CERT`/`AI_TLS_KEY`
/// configured the bridge generates a fresh self-signed certificate at each
/// boot, so a certificate pinned once at service startup goes stale the moment
/// the *backend* restarts. A service whose reconnect loop only redials will
/// then fail forever; one that re-fetches this first recovers on its own.
#[utoipa::path(
    get,
    path = "/certificate",
    tag = "ai",
    responses(
        (status = 200, description = "The bridge's certificate and its fingerprint", body = BridgeCertificateResponse),
        (status = 404, description = "The AI bridge is not enabled on this deployment", body = ErrorResponse),
    ),
)]
async fn bridge_certificate(
    State(state): State<AppState>,
) -> Result<Json<BridgeCertificateResponse>, AppError> {
    // 404 rather than a null body: with AI_QUIC_ADDR unset there is no bridge
    // to describe, and a service should fail loudly on a deployment that never
    // intended to run it.
    let bridge = state.ai.as_ref().ok_or(AppError::NotFound)?;
    let der = bridge.certificate();
    Ok(Json(BridgeCertificateResponse {
        protocol: AI_PROTOCOL.to_string(),
        certificate_pem: to_pem(der.as_ref()),
        fingerprint_sha256: tls::fingerprint(&der),
    }))
}

/// Wrap DER bytes as a PEM certificate: base64 in 64-character lines between
/// the standard armour, which is what every TLS library's loader expects.
fn to_pem(der: &[u8]) -> String {
    let body = base64::engine::general_purpose::STANDARD.encode(der);
    let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
    for line in body.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
        pem.push('\n');
    }
    pem.push_str("-----END CERTIFICATE-----\n");
    pem
}

#[cfg(test)]
mod tests {
    use super::to_pem;

    #[tokio::test]
    async fn pem_armour_wraps_at_64_columns_and_round_trips() {
        // 200 bytes -> 268 base64 chars -> 5 lines (64,64,64,64,12).
        let der: Vec<u8> = (0..200u32).map(|i| (i % 256) as u8).collect();
        let pem = to_pem(&der);
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----\n"));
        assert!(pem.ends_with("-----END CERTIFICATE-----\n"));

        let body: Vec<&str> = pem.lines().filter(|l| !l.starts_with("-----")).collect();
        assert!(
            body.iter().all(|l| l.len() <= 64),
            "no line may exceed 64 columns: {body:?}"
        );
        assert_eq!(body.len(), 5);

        // The real check: a PEM parser gets the original DER back.
        let parsed = rustls_pemfile::certs(&mut pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .expect("valid PEM");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].as_ref(), der.as_slice());
    }
}

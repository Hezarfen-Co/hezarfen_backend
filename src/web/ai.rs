//! Public discovery for the AI bridge.
//!
//! Two reads: the bridge's certificate, which a service needs before it can
//! dial in, and the capabilities currently on offer, which a client needs
//! before it offers an AI feature it cannot get an answer from.
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

use std::collections::BTreeMap;

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
use crate::web::CurrentUser;

pub fn routes() -> OpenApiRouter<AppState> {
    // One `routes!` per path: the multi-handler form groups methods under a
    // single path, so two `get` handlers never share one.
    OpenApiRouter::new()
        .routes(routes!(bridge_certificate))
        .routes(routes!(capabilities))
}

/// Everything a service needs to reach the bridge, except the shared token
/// (which is a secret and is configured out of band).
#[derive(Serialize, ToSchema)]
pub struct BridgeCertificateResponse {
    /// Wire protocol and ALPN the service must announce, currently `hab/2`.
    #[schema(example = "hab/2")]
    protocol: String,
    /// The listener's leaf certificate, PEM-encoded — feed straight into a TLS
    /// client's trust store (Python: `ssl_context.load_verify_locations(cadata=...)`).
    certificate_pem: String,
    /// SHA-256 of the DER certificate, hex. Same value the backend logs at
    /// startup; compare it if the PEM is pinned from somewhere else.
    #[schema(example = "6745e87f8a995e57b58297ac8058cd634d59557f3061144fa61127c96136e7fe")]
    fingerprint_sha256: String,
}

/// The AI bridge's certificate (PEM + SHA-256 fingerprint) for a service to
/// pin; `404` when the bridge is off.
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

/// What the bridge can currently do, grouped by capability.
#[derive(Serialize, ToSchema)]
pub struct CapabilitiesResponse {
    /// Whether a bridge is listening at all (`AI_QUIC_ADDR` set). `false` here
    /// is a discovery answer, not an error: the deployment simply runs no AI.
    enabled: bool,
    /// The wire protocol the bridge speaks, currently `hab/2`.
    #[schema(example = "hab/2")]
    protocol: String,
    /// One entry per capability any connected worker serves, sorted by name.
    capabilities: Vec<CapabilityWorkers>,
}

/// One capability's live fleet: how many workers serve it, and the work in
/// flight across them.
#[derive(Serialize, ToSchema)]
pub struct CapabilityWorkers {
    /// The capability string a service declared, e.g. `chat.reply` or
    /// `rag.chat`.
    #[schema(example = "chat.reply")]
    capability: String,
    /// How many connected workers have declared this capability.
    workers: usize,
    /// Requests currently being answered across those workers.
    inflight: u64,
}

/// The capabilities connected AI services are serving right now, per
/// capability, with the work in flight — a discovery read for an operator or a
/// service deciding what it may ask this bridge to do.
///
/// **`200`, even with the bridge off.** This lists what is available rather
/// than probing a dependency, so a disabled bridge answers `enabled: false`
/// and an empty list, not a `503` — a caller learns *that* there is no AI
/// fleet rather than that the request itself failed.
#[utoipa::path(
    get,
    path = "/capabilities",
    tag = "ai",
    responses(
        (status = 200, description = "Every capability a connected worker serves, grouped and sorted", body = CapabilitiesResponse),
        (status = 401, description = "No authenticated session", body = ErrorResponse),
    ),
)]
async fn capabilities(
    State(state): State<AppState>,
    _user: CurrentUser,
) -> Json<CapabilitiesResponse> {
    let (enabled, workers) = match &state.ai {
        Some(bridge) => (true, bridge.workers()),
        None => (false, Vec::new()),
    };
    // A worker serving N capabilities counts once toward each of them, so the
    // group totals are per-capability fleets, not a partition of the workers.
    let mut by_capability: BTreeMap<String, (usize, u64)> = BTreeMap::new();
    for worker in workers {
        for capability in &worker.capabilities {
            let entry = by_capability.entry(capability.clone()).or_insert((0, 0));
            entry.0 += 1;
            entry.1 += worker.inflight as u64;
        }
    }
    Json(CapabilitiesResponse {
        enabled,
        protocol: AI_PROTOCOL.to_string(),
        capabilities: by_capability
            .into_iter()
            .map(|(capability, (workers, inflight))| CapabilityWorkers {
                capability,
                workers,
                inflight,
            })
            .collect(),
    })
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

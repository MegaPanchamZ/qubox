use axum::{
    extract::State,
    http::StatusCode,
    response::Json,
};

use crate::SignalingState;

/// `GET /v1/webtransport/cert`
/// Returns the SHA-256(DER) hash for `serverCertificateHashes`.
pub(crate) async fn cert_handler(
    State(state): State<SignalingState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let hash = state
        .webtransport_cert_hash()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(hash))
}

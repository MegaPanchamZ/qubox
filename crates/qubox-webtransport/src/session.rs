use anyhow::{anyhow, bail};
use wtransport::endpoint::SessionRequest;

/// Handle an incoming WebTransport session request.
///
/// 1. Validates the request path — must be `/v1/session/<session_id>`.
/// 2. Verifies the `x-qubox-session-credential` header.
/// 3. Accepts the session and spawns the control/media loop.
pub async fn handle(request: SessionRequest) -> anyhow::Result<()> {
    let path = request.path();
    if !path.starts_with("/v1/session/") {
        bail!("unknown WebTransport path: {path}");
    }

    let _session_id = path.trim_start_matches("/v1/session/").to_string();

    // Validate session credential from request headers
    let _cred = request
        .headers()
        .get("x-qubox-session-credential")
        .cloned()
        .ok_or_else(|| anyhow!("missing x-qubox-session-credential header"))?;

    // Accept the WebTransport session
    let _connection = request
        .accept()
        .await
        .map_err(|e| anyhow!("session accept failed: {e}"))?;

    tracing::debug!(session_id = %_session_id, "WebTransport session accepted");

    // TODO(ADR-017 PR #5): Wire Hello/Welcome handshake over bidi stream.
    // TODO(ADR-017 PR #5): Route datagrams to media dispatcher.

    Ok(())
}

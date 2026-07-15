//! Managed enrollment lookup → accounts API for tenant isolation.

use std::sync::Arc;

use qubox_signaling::{DeviceEnrollmentLookup, EnrolledDevice};
use uuid::Uuid;

#[derive(Clone)]
pub struct HttpEnrollmentLookup {
    accounts_url: String,
    admin_token: String,
    client: reqwest::Client,
}

impl HttpEnrollmentLookup {
    pub fn new(accounts_url: impl Into<String>, admin_token: impl Into<String>) -> Self {
        Self {
            accounts_url: accounts_url.into().trim_end_matches('/').to_string(),
            admin_token: admin_token.into(),
            client: reqwest::Client::new(),
        }
    }
}

impl DeviceEnrollmentLookup for HttpEnrollmentLookup {
    fn lookup(
        &self,
        device_id: Uuid,
        public_key: [u8; 32],
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<EnrolledDevice>, String>> + Send + '_>,
    > {
        let url = format!(
            "{}/v1/internal/devices/lookup?device_id={}&public_key_hex={}",
            self.accounts_url,
            device_id,
            hex::encode(public_key)
        );
        let token = self.admin_token.clone();
        let client = self.client.clone();
        Box::pin(async move {
            let res = client
                .get(&url)
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if res.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(None);
            }
            if !res.status().is_success() {
                let status = res.status();
                let body = res.text().await.unwrap_or_default();
                return Err(format!("accounts lookup {status}: {body}"));
            }
            #[derive(serde::Deserialize)]
            struct Body {
                tenant_id: Uuid,
                account_id: Uuid,
                revoked: bool,
            }
            let body: Body = res.json().await.map_err(|e| e.to_string())?;
            Ok(Some(EnrolledDevice {
                tenant_id: body.tenant_id,
                account_id: body.account_id,
                revoked: body.revoked,
            }))
        })
    }
}

pub fn policy_from_env() -> qubox_signaling::EnrollmentPolicy {
    let require = std::env::var("QUBOX_REQUIRE_ENROLLMENT")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);
    if !require {
        return qubox_signaling::EnrollmentPolicy::Open;
    }
    let accounts =
        std::env::var("QUBOX_ACCOUNTS_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let token = std::env::var("QUBOX_ADMIN_TOKEN").unwrap_or_default();
    if token.is_empty() {
        tracing::error!(
            "QUBOX_REQUIRE_ENROLLMENT set but QUBOX_ADMIN_TOKEN empty — falling back to Open"
        );
        return qubox_signaling::EnrollmentPolicy::Open;
    }
    tracing::info!(%accounts, "managed enrollment enabled (tenant isolation)");
    qubox_signaling::EnrollmentPolicy::Managed {
        lookup: Arc::new(HttpEnrollmentLookup::new(accounts, token)),
    }
}

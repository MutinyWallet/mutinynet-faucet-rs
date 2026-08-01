use log::info;
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::{AppState, MAX_SEND_AMOUNT};

#[derive(Clone, Deserialize)]
pub struct ArkadeRequest {
    pub address: String,
    pub sats: u64,
}

#[derive(Clone, Serialize)]
pub struct ArkadeResponse {
    pub txid: String,
}

pub async fn dispense_arkade(
    state: &AppState,
    x_forwarded_for: &str,
    user: &AuthUser,
    payload: ArkadeRequest,
) -> anyhow::Result<ArkadeResponse> {
    let daemon_url = state
        .arkade_daemon_url
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Arkade daemon not configured"))?;

    if payload.sats == 0 {
        anyhow::bail!("sats must be positive");
    }
    if payload.sats > MAX_SEND_AMOUNT {
        anyhow::bail!("max amount is 1,000,000");
    }

    // Atomically check the limits and record the payment before dispensing.
    // Premium users bypass the limit but are still tracked.
    if user.is_premium {
        state
            .payments
            .add_payment(x_forwarded_for, None, Some(user), payload.sats)
            .await;
    } else if !state
        .payments
        .try_reserve_payment(x_forwarded_for, None, Some(user), payload.sats)
        .await
    {
        anyhow::bail!("Too many payments");
    }

    // Do not leak the internal daemon URL or its response body to clients.
    let daemon_url = daemon_url.trim_end_matches('/');
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let mut req = client
        .post(format!("{daemon_url}/send"))
        .json(&serde_json::json!({ "address": payload.address, "sats": payload.sats }));

    if let Some(token) = state.arkade_internal_token.as_deref() {
        req = req.header("X-Internal-Token", token);
    }

    let resp = req.send().await.map_err(|e| {
        log::error!("arkade daemon request failed: {e}");
        anyhow::anyhow!("arkade dispenser unavailable")
    })?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        log::error!("arkade daemon returned {status}: {body}");
        anyhow::bail!("arkade dispenser unavailable");
    }

    let json: serde_json::Value = resp.json().await?;
    let txid = json
        .get("txid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("arkade daemon returned no txid"))?
        .to_string();

    info!(
        "arkade dispensed {} sats to {} for gh:{}",
        payload.sats, payload.address, user.username
    );

    if let Some(tx) = &state.analytics_writer {
        crate::analytics::record_payment(
            tx,
            "arkade",
            payload.sats,
            Some(&user.username),
            x_forwarded_for,
            Some(&payload.address),
        );
    }

    Ok(ArkadeResponse { txid })
}

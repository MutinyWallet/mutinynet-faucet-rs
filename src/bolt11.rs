use serde::{Deserialize, Serialize};
use tonic_openssl_lnd::lnrpc;

use crate::AppState;

#[derive(Clone, Deserialize)]
pub struct Bolt11Request {
    amount_sats: Option<u64>,
}

#[derive(Clone, Serialize)]
pub struct Bolt11Response {
    pub bolt11: String,
}

pub async fn request_bolt11(state: &AppState, payload: Bolt11Request) -> anyhow::Result<String> {
    let mut lightning_client = state.lightning_client.clone();

    let mut inv = lnrpc::Invoice {
        ..Default::default()
    };

    if let Some(amt) = payload.amount_sats {
        inv.value = amt as i64;
    }

    let response = lightning_client.add_invoice(inv).await?.into_inner();
    let bolt11 = response.payment_request;

    if let Some(tx) = &state.analytics_writer {
        crate::analytics::record_payment(
            tx,
            "bolt11",
            payload.amount_sats.unwrap_or(0),
            None,
            "n/a",
            Some(&bolt11),
        );
    }

    Ok(bolt11)
}

use lightning_invoice::Invoice;
use serde::{Deserialize, Serialize};

use std::str::FromStr;
use std::sync::{Arc, Mutex};

use crate::{AppState, MAX_SEND_AMOUNT};

#[derive(Clone, Deserialize)]
pub struct Bolt11Request {
    amount_sats: u64
}

#[derive(Clone, Serialize)]
pub struct Bolt11Response {
    pub bolt11: String,
}

pub async fn request_bolt11(
    state: Arc<Mutex<AppState>>,
    payload: Bolt11Request,
) -> anyhow::Result<String> {
    let bolt11 = {
        let mut lightning_client = state
        .clone()
        .lock()
        .map_err(|_| anyhow::anyhow!("failed to get lock"))?
        .lightning_client
        .clone();

        let response = lightning_client.add_invoice(
            tonic_lnd::lnrpc::Invoice {
                value: payload.amount_sats as i64,
                ..Default::default()
            }

        )
        .await?
        .into_inner();

        response.payment_request
    };

    Ok(bolt11)
}

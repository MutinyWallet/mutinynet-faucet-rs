


use lightning_invoice::Invoice;
use serde::{Deserialize, Serialize};
use tokio::task;
use std::{env, str::FromStr};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tonic_lnd::{LightningClient};

use crate::{MAX_SEND_AMOUNT, AppState};

#[derive(Clone, Deserialize)]
pub struct LightningRequest {
    bolt11: String,
}

#[derive(Clone, Serialize)]
pub struct LightningResponse {
    pub payment_hash: String,
}

pub async fn pay_lightning(
    state: Arc<Mutex<AppState>>,
    payload: LightningRequest,
) -> anyhow::Result<String> {

    if let Ok(invoice) = Invoice::from_str(&payload.bolt11) {
        if let Some(msat_amount) = invoice.amount_milli_satoshis(){
            if msat_amount / 1000 > MAX_SEND_AMOUNT {
                anyhow::bail!("max amount is 1,000,000");
            }
        } else {
            anyhow::bail!("bolt11 invoice should have an amount");
        }
    } else {
        anyhow::bail!("invalid bolt11");
    }

    let payment_hash = {
        let mut lightning_client = state
            .clone()
            .lock()
            .map_err(|_| anyhow::anyhow!("failed to get lock"))?
            .lightning_client
            .clone();

        let response = lightning_client
            .send_payment_sync(tonic_lnd::lnrpc::SendRequest {
                payment_request: payload.bolt11,
                ..Default::default()
            })
            .await?
            .into_inner();

        if response.payment_error != "" {
            return Err(anyhow::anyhow!("Payment error: {}", response.payment_error));
        }

        response.payment_hash
    };

    let hex_payment_hash = hex::encode(payment_hash.clone());

    Ok(hex_payment_hash)
}
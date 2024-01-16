use serde::{Deserialize, Serialize};

use bitcoin_waila::PaymentParams;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tonic_openssl_lnd::lnrpc;

use crate::{AppState, MAX_SEND_AMOUNT};

#[derive(Clone, Deserialize)]
pub struct LightningRequest {
    pub bolt11: String,
}

#[derive(Clone, Serialize)]
pub struct LightningResponse {
    pub payment_hash: String,
}

pub async fn pay_lightning(state: Arc<Mutex<AppState>>, bolt11: &str) -> anyhow::Result<String> {
    let params = PaymentParams::from_str(bolt11).map_err(|_| anyhow::anyhow!("invalid bolt 11"))?;

    let invoice = if let Some(invoice) = params.invoice() {
        if let Some(msat_amount) = invoice.amount_milli_satoshis() {
            if msat_amount / 1000 > MAX_SEND_AMOUNT {
                anyhow::bail!("max amount is 10,000,000");
            }
            invoice
        } else {
            anyhow::bail!("bolt11 invoice should have an amount");
        }
    } else {
        anyhow::bail!("invalid bolt11");
    };

    let payment_preimage = {
        let mut lightning_client = state
            .clone()
            .lock()
            .map_err(|_| anyhow::anyhow!("failed to get lock"))?
            .lightning_client
            .clone();

        let response = lightning_client
            .send_payment_sync(lnrpc::SendRequest {
                payment_request: invoice.to_string(),
                ..Default::default()
            })
            .await?
            .into_inner();

        if !response.payment_error.is_empty() {
            return Err(anyhow::anyhow!("Payment error: {}", response.payment_error));
        }

        response.payment_preimage
    };

    Ok(hex::encode(payment_preimage))
}

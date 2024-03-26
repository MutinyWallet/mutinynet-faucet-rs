use serde::{Deserialize, Serialize};

use bitcoin_waila::PaymentParams;
use lightning_invoice::Bolt11Invoice;
use lnurl::LnUrlResponse;
use std::str::FromStr;
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

pub async fn pay_lightning(state: AppState, bolt11: &str) -> anyhow::Result<String> {
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
    } else if let Some(lnurl) = params.lnurl() {
        match state.lnurl.make_request(&lnurl.url).await? {
            LnUrlResponse::LnUrlPayResponse(pay) => {
                if pay.min_sendable > MAX_SEND_AMOUNT {
                    anyhow::bail!("max amount is 10,000,000");
                }
                let inv = state
                    .lnurl
                    .get_invoice(&pay, pay.min_sendable, None, None)
                    .await?;
                Bolt11Invoice::from_str(inv.invoice())?
            }
            _ => anyhow::bail!("invalid lnurl"),
        }
    } else {
        anyhow::bail!("invalid bolt11")
    };

    let payment_preimage = {
        let mut lightning_client = state.lightning_client.clone();

        let response = lightning_client
            .send_payment_sync(lnrpc::SendRequest {
                payment_request: invoice.to_string(),
                allow_self_payment: true,
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

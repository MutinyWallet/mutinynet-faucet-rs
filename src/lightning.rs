use serde::{Deserialize, Serialize};

use bitcoin_waila::PaymentParams;
use lightning_invoice::Bolt11Invoice;
use lnurl::lightning_address::LightningAddress;
use lnurl::lnurl::LnUrl;
use lnurl::LnUrlResponse;
use nostr::prelude::ZapRequestData;
use nostr::{EventBuilder, Filter, JsonUtil, Kind, Metadata, UncheckedUrl};
use std::str::FromStr;
use tonic_openssl_lnd::lnrpc;

use crate::nostr_dms::RELAYS;
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
    } else if let Some(npub) = params.nostr_pubkey() {
        let client = nostr_sdk::Client::default();
        client.add_relays(RELAYS).await?;
        client.connect().await;

        let filter = Filter::new()
            .author(npub.into())
            .kind(Kind::Metadata)
            .limit(1);
        let events = client.get_events_of(vec![filter], None).await?;
        let event = events
            .into_iter()
            .max_by_key(|e| e.created_at)
            .ok_or(anyhow::anyhow!("no event"))?;

        let metadata = Metadata::from_json(&event.content)?;
        let lnurl = metadata
            .lud16
            .and_then(|l| LightningAddress::from_str(&l).ok().map(|l| l.lnurl()))
            .or(metadata.lud06.and_then(|l| LnUrl::decode(l).ok()))
            .ok_or(anyhow::anyhow!("no lnurl"))?;

        match state.lnurl.make_request(&lnurl.url).await? {
            LnUrlResponse::LnUrlPayResponse(pay) => {
                if pay.min_sendable > MAX_SEND_AMOUNT {
                    anyhow::bail!("max amount is 10,000,000");
                }

                let relays = RELAYS.iter().map(|r| UncheckedUrl::new(*r));
                let zap_data = ZapRequestData::new(npub.into(), relays)
                    .lnurl(lnurl.encode())
                    .amount(pay.min_sendable);
                let zap = EventBuilder::public_zap_request(zap_data).to_event(&state.keys)?;

                let inv = state
                    .lnurl
                    .get_invoice(&pay, pay.min_sendable, Some(zap.as_json()), None)
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

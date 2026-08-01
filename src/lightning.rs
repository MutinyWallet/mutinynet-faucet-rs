use serde::{Deserialize, Serialize};

use bitcoin_waila::PaymentParams;
use lightning_invoice::Bolt11Invoice;
use lnurl::lightning_address::LightningAddress;
use lnurl::lnurl::LnUrl;
use lnurl::LnUrlResponse;
use log::info;
use nostr::prelude::ZapRequestData;
use nostr::{EventBuilder, Filter, JsonUtil, Kind, Metadata, UncheckedUrl};
use std::str::FromStr;
use tonic_openssl_lnd::lnrpc;

use crate::auth::AuthUser;
use crate::nostr_dms::RELAYS;
use crate::{AppState, MAX_SEND_AMOUNT};

/// Max send amount in millisatoshis (`MAX_SEND_AMOUNT` is in sats).
const MAX_SEND_AMOUNT_MSATS: u64 = MAX_SEND_AMOUNT * 1_000;

/// Reject invoices without an amount or with an amount above `max_msats`.
fn validate_invoice_amount(invoice: &Bolt11Invoice, max_msats: u64) -> anyhow::Result<()> {
    let msats = invoice
        .amount_milli_satoshis()
        .ok_or_else(|| anyhow::anyhow!("bolt11 invoice should have an amount"))?;
    if msats > max_msats {
        anyhow::bail!("max amount is 1,000,000");
    }
    Ok(())
}

#[derive(Clone, Deserialize)]
pub struct LightningRequest {
    pub bolt11: String,
}

#[derive(Clone, Serialize)]
pub struct LightningResponse {
    pub payment_hash: String,
}

pub async fn pay_lightning(
    state: &AppState,
    x_forwarded_for: &str,
    user: Option<&AuthUser>,
    bolt11: &str,
) -> anyhow::Result<String> {
    let params = PaymentParams::from_str(bolt11).map_err(|_| anyhow::anyhow!("invalid bolt 11"))?;

    let invoice = if let Some(invoice) = params.invoice() {
        validate_invoice_amount(&invoice, MAX_SEND_AMOUNT_MSATS)?;
        invoice
    } else if let Some(lnurl) = params.lnurl() {
        match state.lnurl.make_request(&lnurl.url).await? {
            LnUrlResponse::LnUrlPayResponse(pay) => {
                if pay.min_sendable > MAX_SEND_AMOUNT_MSATS {
                    anyhow::bail!("max amount is 1,000,000");
                }
                let inv = state
                    .lnurl
                    .get_invoice(&pay, pay.min_sendable, None, None)
                    .await?;
                let invoice = Bolt11Invoice::from_str(inv.invoice())?;
                // A malicious LNURL server can return an invoice for a
                // different amount than requested; never pay more than requested.
                validate_invoice_amount(&invoice, pay.min_sendable)?;
                invoice
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
                if pay.min_sendable > MAX_SEND_AMOUNT_MSATS {
                    anyhow::bail!("max amount is 1,000,000");
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
                let invoice = Bolt11Invoice::from_str(inv.invoice())?;
                // A malicious LNURL server can return an invoice for a
                // different amount than requested; never pay more than requested.
                validate_invoice_amount(&invoice, pay.min_sendable)?;
                invoice
            }
            _ => anyhow::bail!("invalid lnurl"),
        }
    } else {
        anyhow::bail!("invalid bolt11")
    };

    let payment_preimage = {
        let mut lightning_client = state.lightning_client.clone();

        info!("Paying invoice {invoice}");

        let amount_sats = invoice.amount_milli_satoshis().unwrap_or(0) / 1000;

        // Atomically check the limits and record the payment before paying.
        // Premium users bypass the limit but are still tracked.
        let premium = user.is_some_and(|u| u.is_premium);
        if premium {
            state
                .payments
                .add_payment(x_forwarded_for, None, user, amount_sats)
                .await;
        } else if !state
            .payments
            .try_reserve_payment(x_forwarded_for, None, user, amount_sats)
            .await
        {
            anyhow::bail!("Too many payments");
        }

        let response = lightning_client
            .send_payment_sync(lnrpc::SendRequest {
                payment_request: invoice.to_string(),
                allow_self_payment: true,
                ..Default::default()
            })
            .await?
            .into_inner();

        if !response.payment_error.is_empty() {
            // LND returned a completed response that explicitly says no
            // payment was made, so this reservation is safe to release. A
            // timeout or transport error above is ambiguous and intentionally
            // remains reserved to avoid paying the same invoice twice.
            if !premium {
                state
                    .payments
                    .release_payment(x_forwarded_for, None, user, amount_sats)
                    .await;
            }
            return Err(anyhow::anyhow!("Payment error: {}", response.payment_error));
        }

        response.payment_preimage
    };

    if let Some(tx) = &state.analytics_writer {
        crate::analytics::record_payment(
            tx,
            "lightning",
            invoice.amount_milli_satoshis().unwrap_or(0) / 1000,
            user.map(|u| u.username.as_str()),
            x_forwarded_for,
            Some(&invoice.to_string()),
        );
    }

    Ok(hex::encode(payment_preimage))
}

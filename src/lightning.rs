use serde::{Deserialize, Serialize};

use bitcoin_waila::PaymentParams;
use lightning_invoice::Bolt11Invoice;
use lnurl::lightning_address::LightningAddress;
use lnurl::lnurl::LnUrl;
use lnurl::pay::{LnURLPayInvoice, PayResponse};
use lnurl::LnUrlResponse;
use log::info;
use nostr::prelude::ZapRequestData;
use nostr::{EventBuilder, Filter, JsonUtil, Kind, Metadata, UncheckedUrl};
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use tonic_openssl_lnd::lnrpc;

use crate::auth::AuthUser;
use crate::nostr_dms::RELAYS;
use crate::{AppState, MAX_SEND_AMOUNT};

/// Max send amount in millisatoshis (`MAX_SEND_AMOUNT` is in sats).
const MAX_SEND_AMOUNT_MSATS: u64 = MAX_SEND_AMOUNT * 1_000;

/// Parse an LNURL fetch URL and reject unsafe schemes and IP literals.
fn validate_fetch_url(url_str: &str) -> anyhow::Result<url::Url> {
    let url = url::Url::from_str(url_str).map_err(|_| anyhow::anyhow!("invalid url"))?;

    match url.scheme() {
        "https" => {}
        _ => anyhow::bail!("url must use https"),
    }

    match url.host() {
        Some(url::Host::Ipv4(ip)) => validate_fetch_ip(IpAddr::V4(ip))?,
        Some(url::Host::Ipv6(ip)) => validate_fetch_ip(IpAddr::V6(ip))?,
        Some(url::Host::Domain(host)) if host.ends_with(".onion") => {
            anyhow::bail!("onion urls require a configured proxy")
        }
        Some(url::Host::Domain(_)) => {}
        None => anyhow::bail!("url must have a host"),
    }

    Ok(url)
}

fn validate_fetch_ip(ip: IpAddr) -> anyhow::Result<()> {
    let disallowed = match ip {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            let cgnat = octets[0] == 100 && (octets[1] & 0xc0) == 64;
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
                || octets[0] == 0
                || octets[0] >= 240
                || cgnat
        }
        IpAddr::V6(ip) => {
            let first = ip.segments()[0];
            let unique_local = first & 0xfe00 == 0xfc00;
            let site_local = first & 0xffc0 == 0xfec0;
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_unicast_link_local()
                || unique_local
                || site_local
                || ip.to_ipv4_mapped().is_some()
        }
    };

    if disallowed {
        anyhow::bail!("url points at a disallowed address");
    }
    Ok(())
}

/// Build a client that cannot follow redirects or re-resolve a validated
/// hostname to a different address between validation and connection.
async fn safe_lnurl_client(url_str: &str) -> anyhow::Result<lnurl::AsyncClient> {
    let url = validate_fetch_url(url_str)?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("url must have a host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("url must have a known port"))?;

    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();

    if matches!(url.host(), Some(url::Host::Domain(_))) {
        let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| anyhow::anyhow!("failed to resolve url host"))?
            .collect();
        if addresses.is_empty() {
            anyhow::bail!("url host did not resolve");
        }
        for address in &addresses {
            validate_fetch_ip(address.ip())?;
        }
        builder = builder.resolve_to_addrs(host, &addresses);
    }

    Ok(lnurl::AsyncClient::from_client(builder.build()?))
}

pub(crate) async fn make_lnurl_request(url: &str) -> anyhow::Result<LnUrlResponse> {
    Ok(safe_lnurl_client(url).await?.make_request(url).await?)
}

pub(crate) async fn get_lnurl_invoice(
    pay: &PayResponse,
    msats: u64,
    zap_request: Option<String>,
) -> anyhow::Result<LnURLPayInvoice> {
    // The callback is supplied by the remote LNURL service and must receive
    // the same validation and DNS pinning as the initial URL.
    Ok(safe_lnurl_client(&pay.callback)
        .await?
        .get_invoice(pay, msats, zap_request, None)
        .await?)
}

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
        match make_lnurl_request(&lnurl.url).await? {
            LnUrlResponse::LnUrlPayResponse(pay) => {
                if pay.min_sendable > MAX_SEND_AMOUNT_MSATS {
                    anyhow::bail!("max amount is 1,000,000");
                }
                let inv = get_lnurl_invoice(&pay, pay.min_sendable, None).await?;
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
        let events = client
            .get_events_of(vec![filter], Some(std::time::Duration::from_secs(10)))
            .await?;
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

        match make_lnurl_request(&lnurl.url).await? {
            LnUrlResponse::LnUrlPayResponse(pay) => {
                if pay.min_sendable > MAX_SEND_AMOUNT_MSATS {
                    anyhow::bail!("max amount is 1,000,000");
                }

                let relays = RELAYS.iter().map(|r| UncheckedUrl::new(*r));
                let zap_data = ZapRequestData::new(npub.into(), relays)
                    .lnurl(lnurl.encode())
                    .amount(pay.min_sendable);
                let zap = EventBuilder::public_zap_request(zap_data).to_event(&state.keys)?;

                let inv = get_lnurl_invoice(&pay, pay.min_sendable, Some(zap.as_json())).await?;
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

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            lightning_client.send_payment_sync(lnrpc::SendRequest {
                payment_request: invoice.to_string(),
                allow_self_payment: true,
                ..Default::default()
            }),
        )
        .await
        .map_err(|_| anyhow::anyhow!("payment timed out"))??
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_https_and_private_ip_literals() {
        assert!(validate_fetch_url("http://example.com/lnurl").is_err());
        assert!(validate_fetch_url("https://127.0.0.1/lnurl").is_err());
        assert!(validate_fetch_url("https://[::1]/lnurl").is_err());
        assert!(validate_fetch_url("https://[fe80::1]/lnurl").is_err());
    }

    #[tokio::test]
    async fn rejects_hostnames_that_resolve_to_private_addresses() {
        assert!(safe_lnurl_client("https://localhost/lnurl").await.is_err());
    }
}

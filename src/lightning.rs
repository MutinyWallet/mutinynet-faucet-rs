use serde::{Deserialize, Serialize};

use lightning_invoice::Bolt11Invoice;
use lnurl::lightning_address::LightningAddress;
use lnurl::lnurl::LnUrl;
use lnurl::pay::{LnURLPayInvoice, PayResponse};
use lnurl::LnUrlResponse;
use log::info;
use nostr::prelude::ZapRequestData;
use nostr::{EventBuilder, Filter, JsonUtil, Kind, Metadata, RelayUrl};
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use tonic_openssl_lnd::lnrpc;
use tonic_openssl_lnd::routerrpc;

use crate::auth::AuthUser;
use crate::nostr_dms::RELAYS;
use crate::payment_instructions::parse_payment_instructions;
use crate::{AppState, MAX_SEND_AMOUNT};

/// Max send amount in millisatoshis (`MAX_SEND_AMOUNT` is in sats).
const MAX_SEND_AMOUNT_MSATS: u64 = MAX_SEND_AMOUNT * 1_000;
const PAYMENT_TIMEOUT_SECONDS: i32 = 60;
const SMALL_PAYMENT_FEE_THRESHOLD_MSAT: u64 = 1_000_000;
const DEFAULT_ROUTING_FEE_PERCENT: u64 = 5;

#[derive(Debug)]
pub(crate) enum PaymentOutcome {
    Succeeded(String),
    Failed(String),
}

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
pub(crate) fn validate_invoice_amount(
    invoice: &Bolt11Invoice,
    max_msats: u64,
) -> anyhow::Result<()> {
    let msats = invoice
        .amount_milli_satoshis()
        .ok_or_else(|| anyhow::anyhow!("bolt11 invoice should have an amount"))?;
    if msats == 0 || msats > max_msats {
        anyhow::bail!("max amount is 1,000,000");
    }
    Ok(())
}

fn validate_lnurl_invoice_amount(
    invoice: &Bolt11Invoice,
    requested_msats: u64,
) -> anyhow::Result<()> {
    validate_invoice_amount(invoice, requested_msats)?;
    if invoice.amount_milli_satoshis() != Some(requested_msats) {
        anyhow::bail!("LNURL invoice amount does not match the requested amount");
    }
    Ok(())
}

fn msats_to_limit_sats(msats: u64) -> u64 {
    msats.div_ceil(1_000)
}

pub(crate) fn invoice_amount_sats(invoice: &Bolt11Invoice) -> anyhow::Result<u64> {
    invoice
        .amount_milli_satoshis()
        .map(msats_to_limit_sats)
        .ok_or_else(|| anyhow::anyhow!("bolt11 invoice should have an amount"))
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
    let params = parse_payment_instructions(bolt11, state.network).await.ok();

    let lnurl_target = bolt11
        .strip_prefix("lightning:")
        .or_else(|| bolt11.strip_prefix("LIGHTNING:"))
        .unwrap_or(bolt11);
    let lnurl = LnUrl::decode(lnurl_target.to_owned()).ok().or_else(|| {
        LightningAddress::from_str(lnurl_target)
            .ok()
            .map(|address| address.lnurl())
    });

    let invoice = if let Some(invoice) = params.and_then(|params| params.invoice) {
        validate_invoice_amount(&invoice, MAX_SEND_AMOUNT_MSATS)?;
        invoice
    } else if let Some(lnurl) = lnurl {
        match make_lnurl_request(&lnurl.url).await? {
            LnUrlResponse::LnUrlPayResponse(pay) => {
                if pay.min_sendable > MAX_SEND_AMOUNT_MSATS {
                    anyhow::bail!("max amount is 1,000,000");
                }
                let inv = get_lnurl_invoice(&pay, pay.min_sendable, None).await?;
                let invoice = Bolt11Invoice::from_str(inv.invoice())
                    .map_err(|error| anyhow::anyhow!("invalid invoice: {error:?}"))?;
                // A malicious LNURL server can return an invoice for a
                // different amount than requested; never pay more than requested.
                validate_lnurl_invoice_amount(&invoice, pay.min_sendable)?;
                invoice
            }
            _ => anyhow::bail!("invalid lnurl"),
        }
    } else if let Ok(npub) = nostr::PublicKey::parse(
        bolt11
            .strip_prefix("nostr:")
            .or_else(|| bolt11.strip_prefix("NOSTR:"))
            .unwrap_or(bolt11),
    ) {
        let client = nostr_sdk::Client::default();
        for relay in RELAYS {
            client.add_relay(relay).await?;
        }
        client.connect().await;

        let filter = Filter::new().author(npub).kind(Kind::Metadata).limit(1);
        let events = client
            .fetch_events(filter, std::time::Duration::from_secs(10))
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

                let relays = RELAYS
                    .iter()
                    .map(|relay| RelayUrl::parse(relay))
                    .collect::<Result<Vec<_>, _>>()?;
                let zap_data = ZapRequestData::new(npub, relays)
                    .lnurl(lnurl.encode())
                    .amount(pay.min_sendable);
                let zap = EventBuilder::public_zap_request(zap_data).sign_with_keys(&state.keys)?;

                let inv = get_lnurl_invoice(&pay, pay.min_sendable, Some(zap.as_json())).await?;
                let invoice = Bolt11Invoice::from_str(inv.invoice())
                    .map_err(|error| anyhow::anyhow!("invalid invoice: {error:?}"))?;
                // A malicious LNURL server can return an invoice for a
                // different amount than requested; never pay more than requested.
                validate_lnurl_invoice_amount(&invoice, pay.min_sendable)?;
                invoice
            }
            _ => anyhow::bail!("invalid lnurl"),
        }
    } else {
        anyhow::bail!("invalid bolt11")
    };

    info!("Paying invoice {}", invoice.payment_hash());

    let amount_sats = invoice_amount_sats(&invoice)?;

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

    let payment_preimage = match send_bolt11_payment(state, &invoice, true).await? {
        PaymentOutcome::Succeeded(preimage) => preimage,
        PaymentOutcome::Failed(reason) => {
            // LND returned a final failure, so no payment was made and the
            // reservation is safe to release. Transport errors remain reserved
            // because their payment outcome is ambiguous.
            if !premium {
                state
                    .payments
                    .release_payment(x_forwarded_for, None, user, amount_sats)
                    .await;
            }
            anyhow::bail!("Payment failed: {reason}")
        }
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

    Ok(payment_preimage)
}

pub(crate) async fn send_bolt11_payment(
    state: &AppState,
    invoice: &Bolt11Invoice,
    allow_self_payment: bool,
) -> anyhow::Result<PaymentOutcome> {
    let request = send_payment_request(invoice, allow_self_payment)?;
    let mut router_client = state.router_client.clone();
    let mut updates = router_client.send_payment_v2(request).await?.into_inner();

    while let Some(payment) = updates.message().await? {
        if payment.status == lnrpc::payment::PaymentStatus::Succeeded as i32
            || payment.status == lnrpc::payment::PaymentStatus::Failed as i32
        {
            return final_payment_result(payment);
        }
    }

    anyhow::bail!("LND payment stream ended without a final status")
}

fn send_payment_request(
    invoice: &Bolt11Invoice,
    allow_self_payment: bool,
) -> anyhow::Result<routerrpc::SendPaymentRequest> {
    let amount_msat = invoice
        .amount_milli_satoshis()
        .ok_or_else(|| anyhow::anyhow!("bolt11 invoice should have an amount"))?;

    Ok(routerrpc::SendPaymentRequest {
        payment_request: invoice.to_string(),
        timeout_seconds: PAYMENT_TIMEOUT_SECONDS,
        fee_limit_msat: default_routing_fee_limit_msat(amount_msat) as i64,
        allow_self_payment,
        no_inflight_updates: true,
        ..Default::default()
    })
}

fn default_routing_fee_limit_msat(amount_msat: u64) -> u64 {
    if amount_msat <= SMALL_PAYMENT_FEE_THRESHOLD_MSAT {
        amount_msat
    } else {
        amount_msat.saturating_mul(DEFAULT_ROUTING_FEE_PERCENT) / 100
    }
}

fn final_payment_result(payment: lnrpc::Payment) -> anyhow::Result<PaymentOutcome> {
    if payment.status == lnrpc::payment::PaymentStatus::Succeeded as i32 {
        if payment.payment_preimage.is_empty() {
            anyhow::bail!("LND reported a successful payment without a preimage");
        }

        return Ok(PaymentOutcome::Succeeded(payment.payment_preimage));
    }

    if payment.status == lnrpc::payment::PaymentStatus::Failed as i32 {
        let reason = lnrpc::PaymentFailureReason::from_i32(payment.failure_reason)
            .map(|reason| format!("{reason:?}"))
            .unwrap_or_else(|| format!("Unknown({})", payment.failure_reason));
        return Ok(PaymentOutcome::Failed(reason));
    }

    anyhow::bail!(
        "LND returned a non-final payment status: {}",
        payment.status
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_INVOICE: &str = "lnbc2500u1pvjluezsp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygspp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdq5xysxxatsyp3k7enxv4jsxqzpu9qrsgquk0rl77nj30yxdy8j9vdx85fkpmdla2087ne0xh8nhedh8w27kyke0lp53ut353s06fv3qfegext0eh0ymjpf39tuven09sam30g4vgpfna3rh";

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

    #[test]
    fn rounds_millisatoshis_up_for_rate_limits() {
        assert_eq!(msats_to_limit_sats(0), 0);
        assert_eq!(msats_to_limit_sats(1), 1);
        assert_eq!(msats_to_limit_sats(999), 1);
        assert_eq!(msats_to_limit_sats(1_000), 1);
        assert_eq!(msats_to_limit_sats(1_001), 2);
    }

    #[test]
    fn routing_fee_limit_matches_legacy_send_payment_default() {
        assert_eq!(default_routing_fee_limit_msat(1), 1);
        assert_eq!(default_routing_fee_limit_msat(1_000_000), 1_000_000);
        assert_eq!(default_routing_fee_limit_msat(1_001_000), 50_050);
        assert_eq!(default_routing_fee_limit_msat(5_000_000_000), 250_000_000);
    }

    #[test]
    fn send_payment_v2_request_has_safe_explicit_defaults() {
        let invoice = Bolt11Invoice::from_str(TEST_INVOICE).unwrap();
        let request = send_payment_request(&invoice, true).unwrap();

        assert_eq!(request.payment_request, TEST_INVOICE);
        assert_eq!(request.timeout_seconds, PAYMENT_TIMEOUT_SECONDS);
        assert_eq!(request.fee_limit_msat, 12_500_000);
        assert!(request.allow_self_payment);
        assert!(request.no_inflight_updates);
    }

    #[test]
    fn successful_payment_returns_hex_preimage_unchanged() {
        let preimage = "01".repeat(32);
        let payment = lnrpc::Payment {
            status: lnrpc::payment::PaymentStatus::Succeeded as i32,
            payment_preimage: preimage.clone(),
            ..Default::default()
        };

        match final_payment_result(payment).unwrap() {
            PaymentOutcome::Succeeded(actual) => assert_eq!(actual, preimage),
            PaymentOutcome::Failed(reason) => panic!("unexpected failure: {reason}"),
        }
    }

    #[test]
    fn successful_payment_requires_preimage() {
        let payment = lnrpc::Payment {
            status: lnrpc::payment::PaymentStatus::Succeeded as i32,
            ..Default::default()
        };

        assert!(final_payment_result(payment)
            .unwrap_err()
            .to_string()
            .contains("without a preimage"));
    }

    #[test]
    fn failed_payment_reports_lnd_failure_reason() {
        let payment = lnrpc::Payment {
            status: lnrpc::payment::PaymentStatus::Failed as i32,
            failure_reason: lnrpc::PaymentFailureReason::FailureReasonNoRoute as i32,
            ..Default::default()
        };

        match final_payment_result(payment).unwrap() {
            PaymentOutcome::Succeeded(_) => panic!("unexpected success"),
            PaymentOutcome::Failed(reason) => assert!(reason.contains("FailureReasonNoRoute")),
        }
    }
}

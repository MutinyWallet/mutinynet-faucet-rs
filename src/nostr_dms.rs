use crate::{AppState, MAX_SEND_AMOUNT};
use bitcoin::Amount;
use bitcoin_waila::PaymentParams;
use lightning_invoice::Bolt11Invoice;
use lnurl::lightning_address::LightningAddress;
use lnurl::lnurl::LnUrl;
use lnurl::LnUrlResponse;
use log::{error, info, warn};
use nostr::nips::nip04;
use nostr::prelude::ZapRequestData;
use nostr::{nips, Event, Filter, JsonUtil, Kind, Metadata, Timestamp, UncheckedUrl};
use nostr_sdk::{Client, RelayPoolNotification};
use std::str::FromStr;
use tonic_openssl_lnd::lnrpc;

pub const RELAYS: [&str; 2] = ["wss://relay.primal.net", "wss://relay.damus.io"];

/// Rate-limit identity shared by all nostr DM payments. Nostr keys are free
/// to mint, so per-pubkey limits alone are not enough.
const NOSTR_DM_GLOBAL_KEY: &str = "nostr_dm";

/// Daily budget for all nostr DM payments combined, in sats.
const NOSTR_DM_DAILY_LIMIT: u64 = 10 * MAX_SEND_AMOUNT;

pub async fn listen_to_nostr_dms(state: AppState) -> anyhow::Result<()> {
    // Reconnect with exponential backoff (reset whenever events flow) so a
    // relay outage or IP ban does not turn into a hot reconnect loop.
    let mut backoff = std::time::Duration::from_secs(1);
    loop {
        let client = Client::new(&state.keys);
        client.add_relays(RELAYS).await?;
        client.connect().await;

        let filter = Filter::new()
            .pubkey(state.keys.public_key())
            .kind(Kind::EncryptedDirectMessage)
            .since(Timestamp::now());

        client.subscribe(vec![filter], None).await;

        let mut notifications = client.notifications();

        while let Ok(notification) = notifications.recv().await {
            backoff = std::time::Duration::from_secs(1);
            match notification {
                RelayPoolNotification::Event { event, .. } => {
                    if event.kind == Kind::EncryptedDirectMessage {
                        info!("Received dm: {}", event.id);
                        tokio::spawn({
                            let state = state.clone();
                            async move {
                                if let Err(e) = handle_event(*event, state).await {
                                    error!("Error processing dm: {e}")
                                }
                            }
                        });
                    } else {
                        warn!("Received unexpected event: {}", event.id);
                    }
                }
                RelayPoolNotification::Shutdown => {
                    warn!("Relay pool shutdown");
                    break;
                }
                RelayPoolNotification::Stop => {}
                RelayPoolNotification::Message { .. } => {}
                RelayPoolNotification::RelayStatus { .. } => {}
            }
        }

        warn!("nostr relay connection closed; reconnecting in {backoff:?}");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(std::time::Duration::from_secs(300));
    }
}

async fn pay_invoice(
    invoice: Bolt11Invoice,
    state: &AppState,
    nostr_pubkey: &str,
) -> anyhow::Result<()> {
    // only pay if invoice has a valid amount
    if invoice
        .amount_milli_satoshis()
        .is_some_and(|amt| amt / 1_000 < MAX_SEND_AMOUNT)
    {
        let amount_sats = invoice.amount_milli_satoshis().unwrap_or(0) / 1_000;

        // Rate-limit DM payments: per-pubkey and against the global DM
        // budget, since nostr keys are free to mint. Atomic check-and-record.
        let keys = [
            (nostr_pubkey, MAX_SEND_AMOUNT),
            (NOSTR_DM_GLOBAL_KEY, NOSTR_DM_DAILY_LIMIT),
        ];
        if !state.payments.try_reserve(&keys, amount_sats).await {
            anyhow::bail!("Too many payments");
        }

        info!("Paying invoice {} from nostr dm", invoice.payment_hash());
        let mut lightning_client = state.lightning_client.clone();

        let payment_result = async {
            let response = tokio::time::timeout(
                std::time::Duration::from_secs(60),
                lightning_client.send_payment_sync(lnrpc::SendRequest {
                    payment_request: invoice.to_string(),
                    ..Default::default()
                }),
            )
            .await
            .map_err(|_| anyhow::anyhow!("payment timed out"))??
            .into_inner();

            if !response.payment_error.is_empty() {
                anyhow::bail!("Payment error: {}", response.payment_error);
            }
            Ok::<_, anyhow::Error>(response)
        }
        .await;

        if let Err(e) = payment_result {
            state.payments.release(&keys, amount_sats).await;
            return Err(e);
        }

        if let Some(tx) = &state.analytics_writer {
            crate::analytics::record_payment(
                tx,
                "nostr_dm",
                invoice.amount_milli_satoshis().unwrap_or(0) / 1000,
                Some(nostr_pubkey),
                nostr_pubkey,
                Some(&invoice.to_string()),
            );
        }

        Ok(())
    } else {
        Err(anyhow::anyhow!("Invalid invoice amount"))
    }
}

async fn get_lnurl(pubkey: nostr::PublicKey) -> anyhow::Result<LnUrl> {
    let client = Client::default();
    client.add_relays(RELAYS).await?;
    client.connect().await;

    let filter = Filter::new().author(pubkey).kind(Kind::Metadata).limit(1);
    let events = client
        .get_events_of(vec![filter], Some(std::time::Duration::from_secs(10)))
        .await?;
    let event = events
        .into_iter()
        .max_by_key(|e| e.created_at)
        .ok_or(anyhow::anyhow!("no event"))?;

    client.disconnect().await?;

    let metadata = Metadata::from_json(&event.content)?;
    let lnurl = metadata
        .lud16
        .and_then(|l| LightningAddress::from_str(&l).ok().map(|l| l.lnurl()))
        .or(metadata.lud06.and_then(|l| LnUrl::decode(l).ok()))
        .ok_or(anyhow::anyhow!("no lnurl"))?;

    Ok(lnurl)
}

async fn get_invoice(
    lnurl: &LnUrl,
    pubkey: nostr::PublicKey,
    state: &AppState,
) -> anyhow::Result<Bolt11Invoice> {
    let invoice = match crate::lightning::make_lnurl_request(&lnurl.url).await? {
        LnUrlResponse::LnUrlPayResponse(pay) => {
            let amount_msats = pay.min_sendable * 2;
            if amount_msats > MAX_SEND_AMOUNT {
                anyhow::bail!("max amount is 1,000,000");
            }

            let relays = RELAYS.iter().map(|r| UncheckedUrl::new(*r));
            let zap_data = ZapRequestData::new(pubkey, relays)
                .lnurl(lnurl.encode())
                .amount(amount_msats)
                .message("This is a private zap 👻");
            let zap = nips::nip57::private_zap_request(zap_data, &state.keys)?;

            let inv = crate::lightning::get_lnurl_invoice(&pay, amount_msats, Some(zap.as_json()))
                .await?;
            Bolt11Invoice::from_str(inv.invoice())?
        }
        _ => anyhow::bail!("invalid lnurl"),
    };

    Ok(invoice)
}

async fn handle_event(event: Event, state: AppState) -> anyhow::Result<()> {
    event.verify()?;
    let pubkey_str = event.pubkey.to_string();
    let decrypted = nip04::decrypt(state.keys.secret_key()?, &event.pubkey, &event.content)?;

    if decrypted.to_lowercase() == "zap me" {
        info!("Zapping");
        let lnurl = get_lnurl(event.pubkey).await?;
        let invoice = get_invoice(&lnurl, event.pubkey, &state).await?;

        pay_invoice(invoice, &state, &pubkey_str).await?;
    } else if decrypted.to_lowercase() == "spam me" {
        info!("Spamming");
        let lnurl = get_lnurl(event.pubkey).await?;

        for _ in 0..25 {
            let invoice = get_invoice(&lnurl, event.pubkey, &state).await?;
            pay_invoice(invoice, &state, &pubkey_str).await?;
        }
    }

    if let Ok(params) = PaymentParams::from_str(&decrypted) {
        if let Some(invoice) = params.invoice() {
            pay_invoice(invoice, &state, &pubkey_str).await?;
        }

        if let Some(address) = params.address() {
            let amount = params.amount().unwrap_or(Amount::from_sat(100_000));

            if amount.to_sat() > MAX_SEND_AMOUNT {
                return Err(anyhow::anyhow!("Amount exceeds max send amount"));
            }

            // Atomic check-and-record against the per-pubkey, per-address,
            // and global DM limits.
            let address_key = address.to_string();
            let keys = [
                (pubkey_str.as_str(), MAX_SEND_AMOUNT),
                (address_key.as_str(), MAX_SEND_AMOUNT),
                (NOSTR_DM_GLOBAL_KEY, NOSTR_DM_DAILY_LIMIT),
            ];
            if !state.payments.try_reserve(&keys, amount.to_sat()).await {
                return Err(anyhow::anyhow!("Too many payments"));
            }

            let send_result = {
                let mut wallet_client = state.lightning_client.clone();
                info!("Sending {amount} to {address} from nostr dm");
                let req = lnrpc::SendCoinsRequest {
                    addr: address.to_string(),
                    amount: amount.to_sat() as i64,
                    spend_unconfirmed: true,
                    sat_per_vbyte: 1,
                    ..Default::default()
                };
                wallet_client.send_coins(req).await.map(|r| r.into_inner())
            };

            let resp = match send_result {
                Ok(resp) => resp,
                Err(e) => {
                    state.payments.release(&keys, amount.to_sat()).await;
                    return Err(e.into());
                }
            };

            let txid = resp.txid;

            if let Some(tx) = &state.analytics_writer {
                crate::analytics::record_payment(
                    tx,
                    "nostr_dm_onchain",
                    amount.to_sat(),
                    Some(&pubkey_str),
                    &pubkey_str,
                    Some(&address.to_string()),
                );
            }

            info!("Sent onchain tx: {txid}");
            return Ok(());
        }

        // can add handling for more types in the future
    }

    Ok(())
}

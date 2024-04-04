use crate::{AppState, MAX_SEND_AMOUNT};
use bitcoin::Amount;
use bitcoin_waila::PaymentParams;
use bitcoincore_rpc::RpcApi;
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

pub const RELAYS: [&str; 8] = [
    "wss://nostr.mutinywallet.com",
    "wss://relay.mutinywallet.com",
    "wss://relay.primal.net",
    "wss://relay.snort.social",
    "wss://relay.nostr.band",
    "wss://eden.nostr.land",
    "wss://nos.lol",
    "wss://relay.damus.io",
];

pub async fn listen_to_nostr_dms(state: AppState) -> anyhow::Result<()> {
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
            match notification {
                RelayPoolNotification::Event { event, .. } => {
                    if event.kind == Kind::EncryptedDirectMessage {
                        info!("Received dm: {}", event.as_json());
                        tokio::spawn({
                            let state = state.clone();
                            async move {
                                if let Err(e) = handle_event(*event, state).await {
                                    error!("Error processing dm: {e}")
                                }
                            }
                        });
                    } else {
                        warn!("Received unexpected event: {}", event.as_json());
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
    }
}

async fn handle_event(event: Event, state: AppState) -> anyhow::Result<()> {
    event.verify()?;
    let decrypted = nip04::decrypt(state.keys.secret_key()?, &event.pubkey, &event.content)?;

    if decrypted.to_lowercase() == "zap me" {
        info!("Zapping");

        let client = nostr_sdk::Client::default();
        client.add_relays(RELAYS).await?;
        client.connect().await;

        let filter = Filter::new()
            .author(event.pubkey)
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

        let invoice = match state.lnurl.make_request(&lnurl.url).await? {
            LnUrlResponse::LnUrlPayResponse(pay) => {
                let amount_msats = pay.min_sendable * 2;
                if amount_msats > MAX_SEND_AMOUNT {
                    anyhow::bail!("max amount is 10,000,000");
                }

                let relays = RELAYS.iter().map(|r| UncheckedUrl::new(*r));
                let zap_data = ZapRequestData::new(event.pubkey, relays)
                    .lnurl(lnurl.encode())
                    .amount(amount_msats)
                    .message("This is a private zap 👻");
                let zap = nips::nip57::private_zap_request(zap_data, &state.keys)?;

                let inv = state
                    .lnurl
                    .get_invoice(&pay, amount_msats, Some(zap.as_json()), None)
                    .await?;
                Bolt11Invoice::from_str(inv.invoice())?
            }
            _ => anyhow::bail!("invalid lnurl"),
        };

        // only pay if invoice has a valid amount
        if invoice
            .amount_milli_satoshis()
            .is_some_and(|amt| amt / 1_000 < MAX_SEND_AMOUNT)
        {
            info!("Paying invoice: {invoice}");
            let mut lightning_client = state.lightning_client.clone();

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

            return Ok(());
        } else {
            return Err(anyhow::anyhow!("Invalid invoice amount"));
        }
    }

    if let Ok(params) = PaymentParams::from_str(&decrypted) {
        if let Some(invoice) = params.invoice() {
            // only pay if invoice has a valid amount
            if invoice
                .amount_milli_satoshis()
                .is_some_and(|amt| amt / 1_000 < MAX_SEND_AMOUNT)
            {
                info!("Paying invoice: {invoice}");
                let mut lightning_client = state.lightning_client.clone();

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

                return Ok(());
            } else {
                return Err(anyhow::anyhow!("Invalid invoice amount"));
            }
        }

        if let Some(address) = params.address() {
            let amount = params.amount().unwrap_or(Amount::from_sat(100_000));

            if amount.to_sat() > MAX_SEND_AMOUNT {
                return Err(anyhow::anyhow!("Amount exceeds max send amount"));
            }

            let txid = state
                .bitcoin_client
                .send_to_address(&address, amount, None, None, None, None, None, None)?;

            info!("Sent onchain tx: {txid}");
            return Ok(());
        }

        // can add handling for more types in the future
    }

    Ok(())
}

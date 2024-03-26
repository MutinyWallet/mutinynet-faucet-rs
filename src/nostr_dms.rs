use crate::{AppState, MAX_SEND_AMOUNT};
use bitcoin::Amount;
use bitcoin_waila::PaymentParams;
use bitcoincore_rpc::RpcApi;
use log::{error, info, warn};
use nostr::nips::nip04;
use nostr::{Event, Filter, JsonUtil, Kind, Timestamp};
use nostr_sdk::{Client, RelayPoolNotification};
use std::str::FromStr;
use tonic_openssl_lnd::lnrpc;

const RELAYS: [&str; 8] = [
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

    if let Ok(params) = PaymentParams::from_str(&decrypted) {
        if let Some(invoice) = params.invoice() {
            // only pay if invoice has a valid amount
            if invoice
                .amount_milli_satoshis()
                .is_some_and(|amt| amt / 1_000 < MAX_SEND_AMOUNT)
            {
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

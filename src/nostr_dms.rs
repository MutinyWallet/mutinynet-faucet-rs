use crate::AppState;
use bitcoin_waila::PaymentParams;
use log::{error, warn};
use nostr::nips::nip04;
use nostr::{Event, Filter, Kind, Timestamp};
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
                        tokio::spawn({
                            let state = state.clone();
                            async move {
                                if let Err(e) = handle_event(*event, state).await {
                                    error!("Error processing dm: {e}")
                                }
                            }
                        });
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
    let decrypted = nip04::decrypt(state.keys.secret_key()?, &event.pubkey, &event.content)?;

    if let Ok(params) = PaymentParams::from_str(&decrypted) {
        if let Some(invoice) = params.invoice() {
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
        // can add handling for more types in the future
    }

    Ok(())
}

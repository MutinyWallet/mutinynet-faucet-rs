use serde::{Deserialize, Serialize};

use std::sync::{Arc, Mutex};
use tonic_openssl_lnd::lnrpc::{self, channel_point};

use crate::{AppState, MAX_SEND_AMOUNT};

#[derive(Clone, Deserialize)]
pub struct ChannelRequest {
    capacity: i64,
    push_amount: i64,
    pubkey: String,
    host: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct ChannelResponse {
    pub txid: String,
}

pub async fn open_channel(
    state: Arc<Mutex<AppState>>,
    payload: ChannelRequest,
) -> anyhow::Result<String> {
    if payload.capacity > MAX_SEND_AMOUNT.try_into().unwrap() {
        anyhow::bail!("max capacity is 10,000,000");
    }
    if payload.push_amount < 0 {
        anyhow::bail!("push_amount must be positive");
    }
    if payload.push_amount > payload.capacity {
        anyhow::bail!("push_amount must be less than or equal to capacity");
    }

    let node_pubkey_result = hex::decode(payload.pubkey.clone());
    let node_pubkey = match node_pubkey_result {
        Ok(pubkey) => pubkey,
        Err(e) => anyhow::bail!("invalid pubkey: {}", e),
    };

    let channel_point = {
        let mut lightning_client = state
            .clone()
            .lock()
            .map_err(|_| anyhow::anyhow!("failed to get lock"))?
            .lightning_client
            .clone();

        if let Some(host) = payload.host {
            lightning_client
                .connect_peer(lnrpc::ConnectPeerRequest {
                    addr: Some(lnrpc::LightningAddress {
                        pubkey: payload.pubkey.clone(),
                        host,
                    }),
                    ..Default::default()
                })
                .await
                .ok();
        }

        lightning_client
            .open_channel_sync(lnrpc::OpenChannelRequest {
                node_pubkey,
                local_funding_amount: payload.capacity,
                push_sat: payload.push_amount,
                ..Default::default()
            })
            .await?
            .into_inner()
    };

    let txid = match channel_point.funding_txid {
        Some(channel_point::FundingTxid::FundingTxidBytes(bytes)) => hex::encode(bytes),
        Some(channel_point::FundingTxid::FundingTxidStr(string)) => string,
        None => anyhow::bail!("failed to open channel"),
    };

    Ok(txid)
}

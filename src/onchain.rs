use bitcoin::{Address, Amount};
use bitcoin_waila::PaymentParams;
use bitcoincore_rpc::RpcApi;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::task;

use crate::{AppState, MAX_SEND_AMOUNT};

#[derive(Clone, Deserialize)]
pub struct OnchainRequest {
    sats: u64,
    address: String,
}

#[derive(Clone, Serialize)]
pub struct OnchainResponse {
    pub txid: String,
}

pub async fn pay_onchain(
    state: Arc<Mutex<AppState>>,
    payload: OnchainRequest,
) -> anyhow::Result<String> {
    if payload.sats > MAX_SEND_AMOUNT {
        anyhow::bail!("max amount is 10,000,000");
    }

    let txid = {
        let network = state
            .lock()
            .map_err(|_| anyhow::anyhow!("failed to get lock"))?
            .network;

        // need to convert from different rust-bitcoin versions
        let params = PaymentParams::from_str(&payload.address)
            .map_err(|_| anyhow::anyhow!("invalid address"))?;
        let address_str = params.address().ok_or(anyhow::anyhow!("invalid address"))?;

        let address =
            Address::from_str(&address_str.to_string()).map_err(|e| anyhow::anyhow!(e))?;

        let address = if address.is_valid_for_network(network) {
            address
        } else {
            anyhow::bail!(
                "invalid address, are you sure that's a {:?} address?",
                network
            )
        };

        let bitcoin_client = state
            .lock()
            .map_err(|_| anyhow::anyhow!("failed to get lock"))?
            .bitcoin_client
            .clone();

        let amount = Amount::from_sat(payload.sats);

        task::block_in_place(|| {
            bitcoin_client.send_to_address(
                &address.assume_checked(), // we just checked it above,
                amount,
                None,
                None,
                None,
                None,
                None,
                None,
            )
        })?
    };

    Ok(txid.to_string())
}

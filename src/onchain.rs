use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use bitcoin::{Amount, Address};
use bitcoincore_rpc::{
    Auth, Client, RpcApi,
};
use lightning_invoice::Invoice;
use serde::{Deserialize, Serialize};
use tokio::task;
use std::{env, str::FromStr};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tonic_lnd::{LightningClient};

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
        anyhow::bail!("max amount is 1,000,000");
    }

    let txid = {
        let network = state.clone().lock()
            .map_err(|_| anyhow::anyhow!("failed to get lock"))?.network;

        let address = Address::from_str(&payload.address).map_err(|e| anyhow::anyhow!(e))?;

        let address = if address.is_valid_for_network(network) {
            address
        } else {
            anyhow::bail!("invalid address, are you sure that's a {:?} address?", network)
        };

        let bitcoin_client = state.clone().lock()
            .map_err(|_| anyhow::anyhow!("failed to get lock"))?.bitcoin_client.clone();

        let amount = Amount::from_sat(payload.sats);

        let txid = task::block_in_place(|| {
            let txid = bitcoin_client
            .send_to_address(
                &address.assume_checked(), // we just checked it above,
                amount, None, None, None, None, None, None,
            );

            txid

        })?;

        txid.clone()
    };

    Ok(txid.to_string())
}
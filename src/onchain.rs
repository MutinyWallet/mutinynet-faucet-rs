use bitcoin::{Address, Amount};
use bitcoin_waila::PaymentParams;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::{AppState, MAX_SEND_AMOUNT};

#[derive(Clone, Deserialize)]
pub struct OnchainRequest {
    sats: Option<u64>,
    address: String,
}

#[derive(Clone, Serialize)]
pub struct OnchainResponse {
    pub txid: String,
    pub address: String,
}

pub async fn pay_onchain(
    state: AppState,
    x_forwarded_for: &str,
    payload: OnchainRequest,
) -> anyhow::Result<OnchainResponse> {
    let res = {
        let network = state.network;

        // need to convert from different rust-bitcoin versions
        let params = PaymentParams::from_str(&payload.address)
            .map_err(|_| anyhow::anyhow!("invalid address"))?;
        let address_str = params.address().ok_or(anyhow::anyhow!("invalid address"))?;

        let address =
            Address::from_str(&address_str.to_string()).map_err(|e| anyhow::anyhow!(e))?;

        let address = if let Ok(address) = address.require_network(network) {
            address
        } else {
            anyhow::bail!(
                "invalid address, are you sure that's a {:?} address?",
                network
            )
        };

        let amount = params
            .amount()
            .or(payload.sats.map(Amount::from_sat))
            .ok_or(anyhow::anyhow!("invalid amount"))?;

        if amount.to_sat() > MAX_SEND_AMOUNT {
            anyhow::bail!("max amount is 10,000,000");
        }

        let resp = {
            let mut wallet_client = state.lightning_client.clone();
            let req = tonic_openssl_lnd::lnrpc::SendCoinsRequest {
                addr: address.to_string(),
                amount: amount.to_sat() as i64,
                spend_unconfirmed: true,
                ..Default::default()
            };
            wallet_client.send_coins(req).await?.into_inner()
        };

        state
            .payments
            .add_payment(x_forwarded_for, amount.to_sat())
            .await;

        OnchainResponse {
            txid: resp.txid,
            address: address.to_string(),
        }
    };

    Ok(res)
}

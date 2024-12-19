use crate::auth::AuthUser;
use crate::{AppState, MAX_SEND_AMOUNT};
use bitcoin::{Address, Amount};
use bitcoin_waila::PaymentParams;
use log::info;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Clone, Deserialize)]
pub struct OnchainRequest {
    pub sats: Option<u64>,
    pub address: String,
}

#[derive(Clone, Serialize)]
pub struct OnchainResponse {
    pub txid: String,
    pub address: String,
}

pub async fn pay_onchain(
    state: &AppState,
    x_forwarded_for: &str,
    user: AuthUser,
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
            anyhow::bail!("max amount is 1,000,000");
        }

        state
            .payments
            .add_payment(
                x_forwarded_for,
                Some(&address),
                Some(&user),
                amount.to_sat(),
            )
            .await;

        let resp = {
            let mut wallet_client = state.lightning_client.clone();
            info!("Sending {amount} to {address}");
            let req = tonic_openssl_lnd::lnrpc::SendCoinsRequest {
                addr: address.to_string(),
                amount: amount.to_sat() as i64,
                spend_unconfirmed: true,
                sat_per_vbyte: 1,
                ..Default::default()
            };
            wallet_client.send_coins(req).await?.into_inner()
        };

        OnchainResponse {
            txid: resp.txid,
            address: address.to_string(),
        }
    };

    Ok(res)
}

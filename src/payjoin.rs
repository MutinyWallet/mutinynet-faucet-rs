use anyhow::{anyhow, Context};
use axum::headers::HeaderMap;
use bitcoin::psbt::Psbt;
use bitcoin::Amount;
use bitcoincore_rpc::RpcApi;
use payjoin::receive::ProvisionalProposal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Cursor;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tonic_openssl_lnd::lnrpc;

use crate::AppState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bip21Request {
    pub amount: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bip21Response {
    pub bip21: String,
}

pub async fn request_bip21(state: Arc<Mutex<AppState>>, value: i64) -> anyhow::Result<String> {
    let bolt11 = {
        let mut lightning_client = state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("failed to get lock"))?
            .lightning_client
            .clone();

        let inv = lnrpc::Invoice {
            value,
            ..Default::default()
        };

        lightning_client
            .add_invoice(inv)
            .await?
            .into_inner()
            .payment_request
    };

    let address = {
        let mut lightning_client = state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("failed to get lock"))?
            .lightning_client
            .clone();

        let req = lnrpc::NewAddressRequest {
            r#type: lnrpc::AddressType::TaprootPubkey.into(),
            ..Default::default()
        };

        lightning_client
            .new_address(req)
            .await?
            .into_inner()
            .address
    };
    let address = payjoin::bitcoin::address::Address::from_str(&address)?.assume_checked();

    let amount = Amount::from_sat(value as u64);

    let host = state
        .try_lock()
        .map_err(|_| anyhow::anyhow!("failed to get lock"))?
        .host
        .clone();

    Ok(format!(
        "{}?amount={}&invoice={bolt11}&pj={host}/v1/payjoin",
        address.to_qr_uri(),
        amount.to_btc()
    ))
}

struct Headers(HeaderMap);

impl payjoin::receive::Headers for Headers {
    fn get_header(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }
}

pub async fn payjoin_request(
    state: Arc<Mutex<AppState>>,
    headers: HeaderMap,
    body: Vec<u8>,
    query: String,
) -> anyhow::Result<String> {
    let body = Cursor::new(body);
    let proposal =
        payjoin::receive::UncheckedProposal::from_request(body, &query, Headers(headers))
            .map_err(|_| anyhow!("failed to parse request"))?;

    let bitcoin_client = state
        .lock()
        .map_err(|_| anyhow::anyhow!("failed to get lock"))?
        .bitcoin_client
        .clone();

    // The network is used for checks later
    let network = state
        .lock()
        .map_err(|_| anyhow::anyhow!("failed to get lock"))?
        .network;

    // Receive Check 1: Can Broadcast
    let proposal = proposal
        .check_can_broadcast(|tx| {
            let raw_tx = bitcoin::consensus::encode::serialize_hex(&tx);
            let mempool_results = bitcoin_client
                .test_mempool_accept(&[raw_tx])
                .expect("Failed to test mempool accept");
            Ok(mempool_results.first().expect("No mempool results").allowed)
        })
        .map_err(|_| anyhow!("Failed to broadcast"))?;
    log::trace!("check1");

    // Receive Check 2: receiver can't sign for proposal inputs
    let proposal = proposal
        .check_inputs_not_owned(|input| {
            if let Ok(address) = bitcoin::Address::from_script(input, network) {
                Ok(bitcoin_client
                    .get_address_info(&address)
                    .map(|info| info.is_mine.unwrap_or(false))
                    .unwrap())
            } else {
                Ok(false)
            }
        })
        .map_err(|_| anyhow!("Failed to sign inputs"))?;
    log::trace!("check2");
    // Receive Check 3: receiver can't sign for proposal inputs
    let proposal = proposal
        .check_no_mixed_input_scripts()
        .map_err(|_| anyhow!("Failed to sign inputs"))?;
    log::trace!("check3");

    // Receive Check 4: have we seen this input before? More of a check for non-interactive i.e. payment processor receivers.
    let payjoin = proposal
        .check_no_inputs_seen_before(|_| Ok(false))
        .map_err(|_| anyhow!("Failed to sign inputs"))?;
    log::trace!("check4");

    let mut provisional_payjoin = payjoin
        .identify_receiver_outputs(|output_script| {
            if let Ok(address) = bitcoin::Address::from_script(output_script, network) {
                Ok(bitcoin_client
                    .get_address_info(&address)
                    .map(|info| info.is_mine.unwrap_or(false))
                    .unwrap())
            } else {
                Ok(false)
            }
        })
        .map_err(|_| anyhow!("Failed to sign inputs"))?;

    // Select receiver payjoin inputs.
    _ = try_contributing_inputs(&mut provisional_payjoin, &bitcoin_client)
        .map_err(|e| log::warn!("Failed to contribute inputs: {}", e));

    let receiver_substitute_address = bitcoin_client.get_new_address(None, None)?.assume_checked();
    provisional_payjoin.substitute_output_address(receiver_substitute_address);

    let payjoin_proposal = provisional_payjoin
        .finalize_proposal(
            |psbt: &Psbt| {
                bitcoin_client
                    .wallet_process_psbt(
                        &payjoin::base64::encode(psbt.serialize()),
                        None,
                        None,
                        Some(false),
                    )
                    .map(|res| {
                        Psbt::from_str(&res.psbt).map_err(|e| payjoin::Error::Server(e.into()))
                    })
                    .map_err(|e| payjoin::Error::Server(e.into()))?
            },
            Some(bitcoin::FeeRate::MIN),
        )
        .map_err(|e| anyhow!("Failed to finalize proposal: {}", e))?;
    let payjoin_proposal_psbt = payjoin_proposal.psbt();
    log::debug!("Receiver's Payjoin proposal PSBT Response: {payjoin_proposal_psbt:#?}");

    let payload = payjoin::base64::encode(payjoin_proposal_psbt.serialize());
    log::info!("successful response");
    Ok(payload)
}

fn try_contributing_inputs(
    payjoin: &mut ProvisionalProposal,
    bitcoind: &bitcoincore_rpc::Client,
) -> anyhow::Result<()> {
    use bitcoin::OutPoint;

    let available_inputs = bitcoind
        .list_unspent(None, None, None, None, None)
        .context("Failed to list unspent from bitcoind")?;
    let candidate_inputs: HashMap<Amount, OutPoint> = available_inputs
        .iter()
        .map(|i| {
            (
                i.amount,
                OutPoint {
                    txid: i.txid,
                    vout: i.vout,
                },
            )
        })
        .collect();

    let selected_outpoint = payjoin
        .try_preserving_privacy(candidate_inputs)
        .map_err(|_| anyhow!("Failed to preserve privacy"))?;
    let selected_utxo = available_inputs
        .iter()
        .find(|i| i.txid == selected_outpoint.txid && i.vout == selected_outpoint.vout)
        .context("This shouldn't happen. Failed to retrieve the privacy preserving utxo from those we provided to the seclector.")?;
    log::debug!("selected utxo: {:#?}", selected_utxo);

    //  calculate receiver payjoin outputs given receiver payjoin inputs and original_psbt,
    let txo_to_contribute = bitcoin::TxOut {
        value: selected_utxo.amount.to_sat(),
        script_pubkey: selected_utxo.script_pub_key.clone(),
    };
    let outpoint_to_contribute = OutPoint {
        txid: selected_utxo.txid,
        vout: selected_utxo.vout,
    };
    payjoin.contribute_witness_input(txo_to_contribute, outpoint_to_contribute);
    Ok(())
}
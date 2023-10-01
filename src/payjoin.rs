use anyhow::{anyhow, Context};
use axum::headers::HeaderMap;
use bitcoin::hashes::Hash;
use bitcoin::psbt::Psbt;
use bitcoin::{Address, Amount, ScriptBuf, Txid};
use bitcoincore_rpc::RpcApi;
use payjoin::receive::ProvisionalProposal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Cursor;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::runtime::Handle;
use tokio::task::block_in_place;
use tonic_openssl_lnd::lnrpc::AddressType;
use tonic_openssl_lnd::walletrpc::fund_psbt_request::{Fees, Template};
use tonic_openssl_lnd::walletrpc::SignPsbtRequest;
use tonic_openssl_lnd::{lnrpc, walletrpc, LndWalletClient};

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

    let address = state
        .try_lock()
        .map_err(|_| anyhow::anyhow!("failed to get lock"))?
        .address
        .clone();

    let amount = Amount::from_sat(value as u64);

    let host = state
        .try_lock()
        .map_err(|_| anyhow::anyhow!("failed to get lock"))?
        .host
        .clone();

    Ok(format!(
        "{}?amount={}&invoice={bolt11}&pj={host}/api/payjoin",
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

    let mut lightning_client = state
        .lock()
        .map_err(|_| anyhow::anyhow!("failed to get lock"))?
        .lightning_client
        .clone();

    let mut wallet_client = state
        .lock()
        .map_err(|_| anyhow::anyhow!("failed to get lock"))?
        .wallet_client
        .clone();

    let fixed_address = state
        .lock()
        .map_err(|_| anyhow::anyhow!("failed to get lock"))?
        .address
        .clone();

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
        .check_inputs_not_owned(|input| Ok(input == &fixed_address.script_pubkey()))
        .map_err(|_| anyhow!("Failed to validate inputs"))?;
    log::trace!("check2");
    // Receive Check 3: receiver can't sign for proposal inputs
    let proposal = proposal
        .check_no_mixed_input_scripts()
        .map_err(|_| anyhow!("Failed to validate input scripts"))?;
    log::trace!("check3");

    // Receive Check 4: have we seen this input before? More of a check for non-interactive i.e. payment processor receivers.
    let payjoin = proposal
        .check_no_inputs_seen_before(|_| Ok(false))
        .map_err(|_| anyhow!("Failed to check no inputs seen"))?;
    log::trace!("check4");

    let mut provisional_payjoin = payjoin
        .identify_receiver_outputs(|output_script| {
            Ok(output_script == &fixed_address.script_pubkey())
        })
        .map_err(|e| {
            eprintln!("Failed to identify receiver outputs: {e}");
            anyhow!("Failed to identify receiver outputs: {e}")
        })?;

    // Select receiver payjoin inputs.
    _ = try_contributing_inputs(&mut provisional_payjoin, &mut wallet_client)
        .await
        .map_err(|e| log::warn!("Failed to contribute inputs: {e}"));

    let receiver_substitute_address = {
        let address = lightning_client
            .new_address(lnrpc::NewAddressRequest {
                r#type: AddressType::TaprootPubkey.into(),
                ..Default::default()
            })
            .await?
            .into_inner()
            .address;
        Address::from_str(&address)?.assume_checked()
    };
    provisional_payjoin.substitute_output_address(receiver_substitute_address);

    let payjoin_proposal = provisional_payjoin
        .finalize_proposal(
            |psbt: &Psbt| {
                let mut wallet_client = state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("failed to get lock"))
                    .map_err(|e| payjoin::Error::Server(e.into()))?
                    .wallet_client
                    .clone();

                block_in_place(move || {
                    Handle::current().block_on(async move {
                        let temp = Template::Psbt(psbt.serialize());
                        let fees = Fees::TargetConf(6);
                        let request = walletrpc::FundPsbtRequest {
                            template: Some(temp),
                            fees: Some(fees),
                            ..Default::default()
                        };
                        let funded = wallet_client.fund_psbt(request).await.unwrap().into_inner();

                        let request = SignPsbtRequest {
                            funded_psbt: funded.funded_psbt,
                        };
                        wallet_client
                            .sign_psbt(request)
                            .await
                            .map(|res| {
                                let res = res.into_inner();
                                Psbt::deserialize(&res.signed_psbt)
                                    .map_err(|e| payjoin::Error::Server(e.into()))
                            })
                            .map_err(|e| payjoin::Error::Server(e.into()))?
                    })
                })
            },
            Some(bitcoin::FeeRate::BROADCAST_MIN),
        )
        .map_err(|e| anyhow!("Failed to finalize proposal: {e}"))?;
    let payjoin_proposal_psbt = payjoin_proposal.psbt();

    let finalized = bitcoin_client.finalize_psbt(&payjoin_proposal_psbt.to_string(), None)?;
    let psbt = Psbt::from_str(&finalized.psbt.unwrap())?;

    log::debug!("Receiver's Payjoin proposal PSBT Response: {psbt:#?}");

    let payload = payjoin::base64::encode(psbt.serialize());
    log::info!("successful response");
    Ok(payload)
}

async fn try_contributing_inputs(
    payjoin: &mut ProvisionalProposal,
    lnd: &mut LndWalletClient,
) -> anyhow::Result<()> {
    use bitcoin::OutPoint;

    let available_inputs = lnd
        .list_unspent(walletrpc::ListUnspentRequest {
            min_confs: 0,
            max_confs: 9999999,
            ..Default::default()
        })
        .await?
        .into_inner()
        .utxos;

    let candidate_inputs: HashMap<Amount, OutPoint> = available_inputs
        .iter()
        .map(|i| {
            (
                Amount::from_sat(i.amount_sat as u64),
                OutPoint {
                    txid: Txid::from_slice(&i.outpoint.as_ref().unwrap().txid_bytes).unwrap(),
                    vout: i.outpoint.as_ref().unwrap().output_index,
                },
            )
        })
        .collect();

    let selected_outpoint = payjoin
        .try_preserving_privacy(candidate_inputs)
        .map_err(|_| anyhow!("Failed to preserve privacy"))?;
    let selected_utxo = available_inputs
        .iter()
        .find(|i| i.outpoint.as_ref().unwrap().txid_bytes == selected_outpoint.txid.to_byte_array().to_vec() && i.outpoint.as_ref().unwrap().output_index == selected_outpoint.vout)
        .context("This shouldn't happen. Failed to retrieve the privacy preserving utxo from those we provided to the selector.")?;
    log::debug!("selected utxo: {:#?}", selected_utxo);

    // calculate receiver payjoin outputs given receiver payjoin inputs and original_psbt,
    let txo_to_contribute = bitcoin::TxOut {
        value: selected_utxo.amount_sat as u64,
        script_pubkey: ScriptBuf::from_hex(&selected_utxo.pk_script).unwrap(),
    };
    let outpoint_to_contribute = OutPoint {
        txid: Txid::from_slice(&selected_utxo.outpoint.as_ref().unwrap().txid_bytes).unwrap(),
        vout: selected_utxo.outpoint.as_ref().unwrap().output_index,
    };

    // Reserve the selected input for the payjoin.
    // We need this to be able to sign the payjoin.
    let outpoint = lnrpc::OutPoint {
        txid_bytes: outpoint_to_contribute.txid.to_byte_array().to_vec(),
        output_index: outpoint_to_contribute.vout,
        ..Default::default()
    };
    let req = walletrpc::LeaseOutputRequest {
        id: b"payjoin".to_vec(),
        outpoint: Some(outpoint),
        expiration_seconds: 30,
    };
    lnd.lease_output(req).await?;

    payjoin.contribute_witness_input(txo_to_contribute, outpoint_to_contribute);
    Ok(())
}

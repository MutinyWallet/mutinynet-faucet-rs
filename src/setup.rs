use bitcoincore_rpc::{Auth, Client, RpcApi};
use tonic_openssl_lnd::lnrpc;

use bitcoin::Address;
use std::env;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tonic_openssl_lnd::lnrpc::AddressType;

use crate::AppState;

pub async fn setup() -> Arc<Mutex<AppState>> {
    // Load environment variables from various sources.
    dotenv::from_filename(".env.local").ok();
    dotenv::from_filename(".env").ok();
    dotenv::dotenv().ok();

    let host = env::var("HOST").expect("missing HOST");

    let network = env::var("NETWORK").expect("missing NETWORK");

    let network = match network {
        network if network == "signet" => bitcoin::Network::Signet,
        network if network == "testnet" => bitcoin::Network::Testnet,
        network if network == "regtest" => bitcoin::Network::Regtest,
        _ => panic!("invalid network"),
    };

    println!("network: {:?}", network);

    // Setup lightning stuff
    let (lightning_client, wallet_client, address) = {
        let address = env::var("GRPC_HOST").expect("missing GRPC_HOST");
        let macaroon_file = env::var("ADMIN_MACAROON_PATH").expect("missing ADMIN_MACAROON_PATH");
        let cert_file = env::var("TLS_CERT_PATH").expect("missing TLS_CERT_PATH");
        let port: u32 = env::var("GRPC_PORT")
            .expect("missing GRPC_PORT")
            .parse()
            .expect("GRPC_PORT must be a number");

        let mut lnd = tonic_openssl_lnd::connect(address, port, cert_file, macaroon_file)
            .await
            .expect("failed to connect");

        let lightning_client = lnd.lightning().clone();

        // Make sure we can get info at startup
        let address = lightning_client
            .clone()
            .new_address(lnrpc::NewAddressRequest {
                r#type: AddressType::TaprootPubkey.into(),
                ..Default::default()
            })
            .await
            .expect("failed to get new address")
            .into_inner()
            .address;
        let address = Address::from_str(&address).unwrap().assume_checked();

        (lightning_client, lnd.wallet().clone(), address)
    };

    // Setup bitcoin rpc stuff
    let bitcoin_client = {
        let url = env::var("BITCOIN_RPC_HOST_AND_PORT").expect("missing BITCOIN_RPC_HOST_AND_PORT");
        let user = env::var("BITCOIN_RPC_USER").expect("missing BITCOIN_RPC_USER");
        let pass = env::var("BITCOIN_RPC_PASSWORD").expect("missing BITCOIN_RPC_PASSWORD");
        let rpc =
            Client::new(&url, Auth::UserPass(user, pass)).expect("failed to create RPC client");

        // Make sure we can get info at startup
        let _blockchain_info = rpc.get_blockchain_info();

        rpc
    };

    let state = AppState::new(
        host,
        lightning_client,
        wallet_client,
        bitcoin_client,
        network,
        address,
    );

    Arc::new(Mutex::new(state))
}

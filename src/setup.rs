use bitcoincore_rpc::{Auth, Client, RpcApi};

use std::env;
use std::sync::{Arc, Mutex};

use crate::AppState;

pub async fn setup() -> Arc<Mutex<AppState>> {
    // Load environment variables from various sources.
    dotenv::from_filename(".env.local").ok();
    dotenv::from_filename(".env").ok();
    dotenv::dotenv().ok();

    let network = env::var("NETWORK").expect("missing NETWORK");

    let network = match network {
        network if network == "signet" => bitcoin::Network::Signet,
        network if network == "testnet" => bitcoin::Network::Testnet,
        network if network == "regtest" => bitcoin::Network::Regtest,
        _ => panic!("invalid network"),
    };

    println!("network: {:?}", network);

    // Setup lightning stuff
    let lightning_client = {
        let address = env::var("GRPC_HOST").expect("missing GRPC_HOST");
        let macaroon_file = env::var("ADMIN_MACAROON_PATH").expect("missing ADMIN_MACAROON_PATH");
        let cert_file = env::var("TLS_CERT_PATH").expect("missing TLS_CERT_PATH");
        let port: u32 = env::var("GRPC_PORT")
            .expect("missing GRPC_PORT")
            .parse()
            .expect("GRPC_PORT must be a number");

        let lightning_client = tonic_lnd::connect(address, port, cert_file, macaroon_file)
            .await
            .expect("failed to connect")
            .lightning()
            .clone();

        // Make sure we can get info at startup
        let _ = lightning_client
            .clone()
            .get_info(tonic_lnd::lnrpc::GetInfoRequest {})
            .await
            .expect("failed to get info");

        lightning_client
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

    let state = AppState::new(lightning_client, bitcoin_client, network);

    Arc::new(Mutex::new(state))
}

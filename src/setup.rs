use std::env;

use nostr::key::Keys;
use tonic_openssl_lnd::lnrpc;

use crate::AppState;

pub async fn setup() -> anyhow::Result<AppState> {
    // Load environment variables from various sources.
    dotenv::from_filename(".env.local").ok();
    dotenv::from_filename(".env").ok();
    dotenv::dotenv().ok();
    // log env logger after dotenv
    pretty_env_logger::try_init()?;

    let host = env::var("HOST").expect("missing HOST");

    // read keys from env, otherwise generate one
    let keys = env::var("NSEC")
        .map(|k| Keys::parse(k).expect("Invalid nsec"))
        .unwrap_or(Keys::generate());

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

        let mut lnd = tonic_openssl_lnd::connect(address, port, cert_file, macaroon_file)
            .await
            .expect("failed to connect");

        let lightning_client = lnd.lightning().clone();

        // Make sure we can get info at startup
        let _ = lightning_client
            .clone()
            .get_info(lnrpc::GetInfoRequest {})
            .await
            .expect("failed to get info")
            .into_inner();

        lightning_client
    };

    Ok(AppState::new(host, keys, lightning_client, network))
}

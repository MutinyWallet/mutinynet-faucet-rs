use std::env;

use nostr::key::Keys;
use tonic_openssl_lnd::lnrpc;

use crate::auth::AuthState;
use crate::AppState;

pub async fn setup() -> anyhow::Result<AppState> {
    // Load environment variables from various sources.
    dotenv::from_filename(".env.local").ok();
    dotenv::from_filename(".env").ok();
    dotenv::dotenv().ok();
    // log env logger after dotenv
    pretty_env_logger::try_init()?;

    let host = env::var("HOST").expect("missing HOST");

    // Load environment variables
    let github_client_id = env::var("GITHUB_CLIENT_ID").expect("GITHUB_CLIENT_ID must be set");
    let github_client_secret =
        env::var("GITHUB_CLIENT_SECRET").expect("GITHUB_CLIENT_SECRET must be set");
    let jwt_secret = env::var("JWT_SECRET").expect("JWT_SECRET must be set");

    if github_client_id.is_empty() {
        panic!("GITHUB_CLIENT_ID must be set");
    }
    if github_client_secret.is_empty() {
        panic!("GITHUB_CLIENT_SECRET must be set");
    }
    if jwt_secret.is_empty() {
        panic!("JWT_SECRET must be set");
    }

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

    let auth = AuthState {
        client: reqwest::Client::new(),
        github_client_id,
        github_client_secret,
        jwt_secret,
    };

    Ok(AppState::new(host, keys, lightning_client, network, auth))
}

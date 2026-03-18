use std::collections::HashMap;
use std::env;
use std::sync::Arc;

use bitcoincore_rpc::Auth;
use log::{info, warn};
use nostr::key::Keys;
use tonic_openssl_lnd::lnrpc;

use crate::analytics::{init_analytics_db, start_write_batcher};
use crate::auth::AuthState;
use crate::l402::L402Config;
use crate::reorg::init_reorg_db;
use crate::{AppState, ReorgConfig};

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

    // Initialize reorg configuration
    let reorg_enabled = env::var("REORG_ENABLED")
        .unwrap_or_else(|_| "false".to_string())
        .parse::<bool>()?;

    let reorg_cooldown_seconds = env::var("REORG_COOLDOWN_SECONDS")
        .unwrap_or_else(|_| "3600".to_string())
        .parse::<u64>()?;

    // Initialize L402 configuration
    let l402_enabled = env::var("L402_ENABLED")
        .unwrap_or_else(|_| "false".to_string())
        .parse::<bool>()?;

    let l402_invoice_amount_sats = env::var("L402_INVOICE_AMOUNT")
        .unwrap_or_else(|_| "1000".to_string())
        .parse::<u64>()?;

    // Initialize mainnet LND client if reorg or L402 is enabled
    let needs_mainnet_lnd = reorg_enabled || l402_enabled;
    let mainnet_lightning_client = if needs_mainnet_lnd {
        let mainnet_address = env::var("MAINNET_GRPC_HOST").ok();
        let mainnet_macaroon = env::var("MAINNET_ADMIN_MACAROON_PATH").ok();
        let mainnet_cert = env::var("MAINNET_TLS_CERT_PATH").ok();
        let mainnet_port_str = env::var("MAINNET_GRPC_PORT").ok();

        match (
            mainnet_address,
            mainnet_macaroon,
            mainnet_cert,
            mainnet_port_str,
        ) {
            (Some(address), Some(macaroon_file), Some(cert_file), Some(port_str)) => {
                let port: u32 = port_str
                    .parse()
                    .expect("MAINNET_GRPC_PORT must be a number");

                info!("Connecting to mainnet LND at {}:{}", address, port);

                let mut mainnet_lnd =
                    tonic_openssl_lnd::connect(address.clone(), port, cert_file, macaroon_file)
                        .await
                        .expect("Failed to connect to mainnet LND");

                let mainnet_client = mainnet_lnd.lightning().clone();

                // Verify connection and check it's mainnet
                let info = mainnet_client
                    .clone()
                    .get_info(lnrpc::GetInfoRequest {})
                    .await
                    .expect("Failed to get mainnet LND info")
                    .into_inner();

                // Verify this is actually mainnet
                let is_mainnet = info
                    .chains
                    .iter()
                    .any(|chain| chain.chain == "bitcoin" && chain.network == "mainnet");

                if !is_mainnet {
                    panic!(
                        "Mainnet LND connection is not on mainnet! Found chains: {:?}",
                        info.chains
                    );
                }

                info!("Successfully connected to mainnet LND");
                Some(mainnet_client)
            }
            _ => {
                warn!("Mainnet LND env vars not set. Features requiring mainnet LND will be disabled.");
                None
            }
        }
    } else {
        None
    };

    // Initialize Bitcoin Core RPC client if reorg is enabled
    let bitcoin_rpc = if reorg_enabled && mainnet_lightning_client.is_some() {
        let rpc_url = env::var("BITCOIN_RPC_HOST_AND_PORT").ok();
        let rpc_user = env::var("BITCOIN_RPC_USER").ok();
        let rpc_password = env::var("BITCOIN_RPC_PASSWORD").ok();

        match (rpc_url, rpc_user, rpc_password) {
            (Some(url), Some(user), Some(password)) => {
                info!("Connecting to Bitcoin Core RPC at {}", url);

                let full_url = if url.starts_with("http") {
                    url
                } else {
                    format!("http://{url}")
                };

                let rpc_client =
                    bitcoincore_rpc::Client::new(&full_url, Auth::UserPass(user, password))
                        .expect("Failed to create Bitcoin Core RPC client");

                info!("Successfully connected to Bitcoin Core",);
                Some(Arc::new(rpc_client))
            }
            _ => {
                warn!("REORG_ENABLED=true but Bitcoin Core RPC env vars not set. Reorg feature will be disabled.");
                None
            }
        }
    } else {
        None
    };

    // Initialize reorg database if feature enabled
    let reorg_db = if reorg_enabled && mainnet_lightning_client.is_some() && bitcoin_rpc.is_some() {
        let db_path = env::var("REORG_DB_PATH").unwrap_or_else(|_| "reorg.db".to_string());
        match init_reorg_db(&db_path).await {
            Ok(pool) => {
                info!("Reorg database initialized");
                Some(pool)
            }
            Err(e) => {
                warn!(
                    "Failed to initialize reorg database: {}. Reorg feature will be disabled.",
                    e
                );
                None
            }
        }
    } else {
        None
    };

    // Final check: only enable if mainnet LND, Bitcoin RPC, and DB are all available
    let reorg_final_enabled = reorg_enabled
        && mainnet_lightning_client.is_some()
        && bitcoin_rpc.is_some()
        && reorg_db.is_some();

    if reorg_enabled && !reorg_final_enabled {
        warn!("Reorg feature requested but not fully configured. Feature disabled.");
    } else if reorg_final_enabled {
        info!(
            "Reorg feature enabled with {} second cooldown",
            reorg_cooldown_seconds
        );
    }

    // Initialize pricing map
    let mut pricing = HashMap::with_capacity(6);
    pricing.insert(1, 10_000);
    pricing.insert(2, 20_000);
    pricing.insert(3, 35_000);
    pricing.insert(4, 50_000);
    pricing.insert(5, 75_000);

    let reorg_config = ReorgConfig {
        enabled: reorg_final_enabled,
        cooldown_seconds: reorg_cooldown_seconds,
        pricing,
    };

    // Finalize L402 config
    let l402_final_enabled = l402_enabled && mainnet_lightning_client.is_some();

    if l402_enabled && !l402_final_enabled {
        warn!("L402 feature requested but mainnet LND not configured. L402 disabled.");
    } else if l402_final_enabled {
        info!(
            "L402 authentication enabled with {} sat invoice amount",
            l402_invoice_amount_sats
        );
    }

    let l402_config = L402Config {
        enabled: l402_final_enabled,
        invoice_amount_sats: l402_invoice_amount_sats,
    };

    // Initialize analytics database
    let analytics_db_path =
        env::var("ANALYTICS_DB_PATH").unwrap_or_else(|_| "analytics.db".to_string());
    let (analytics_db, analytics_writer) = match init_analytics_db(&analytics_db_path).await {
        Ok(pool) => {
            info!("Analytics database initialized at {}", analytics_db_path);
            let writer = start_write_batcher(pool.clone());
            (Some(pool), Some(writer))
        }
        Err(e) => {
            warn!(
                "Failed to initialize analytics database: {}. Analytics disabled.",
                e
            );
            (None, None)
        }
    };

    let analytics_token = env::var("ANALYTICS_TOKEN").ok();
    if analytics_token.is_some() {
        info!("Analytics API token configured");
    } else if analytics_db.is_some() {
        warn!("ANALYTICS_TOKEN not set — analytics endpoints will return 404");
    }

    Ok(AppState::new(
        host,
        keys,
        lightning_client,
        mainnet_lightning_client,
        bitcoin_rpc,
        reorg_db,
        network,
        auth,
        reorg_config,
        l402_config,
        analytics_db,
        analytics_writer,
        analytics_token,
    ))
}

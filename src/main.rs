use std::collections::HashMap;
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use axum::body::Bytes;
use axum::extract::Query;
use axum::headers::HeaderMap;
use axum::http::Uri;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bitcoin::Address;
use bitcoincore_rpc::Client;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::oneshot;
use tonic_openssl_lnd::{LndLightningClient, LndWalletClient};
use tower_http::cors::{AllowHeaders, AllowMethods, Any, CorsLayer};

use bolt11::{request_bolt11, Bolt11Request, Bolt11Response};
use channel::{open_channel, ChannelRequest, ChannelResponse};
use lightning::{pay_lightning, LightningRequest, LightningResponse};
use onchain::{pay_onchain, OnchainRequest, OnchainResponse};
use setup::setup;

mod bolt11;
mod channel;
mod lightning;
mod onchain;
mod payjoin;
mod setup;

pub struct AppState {
    pub host: String,
    network: bitcoin::Network,
    lightning_client: LndLightningClient,
    wallet_client: LndWalletClient,
    bitcoin_client: Arc<Client>,
    address: Address,
}

impl AppState {
    pub fn new(
        host: String,
        lightning_client: LndLightningClient,
        wallet_client: LndWalletClient,
        bitcoin_client: Client,
        network: bitcoin::Network,
        address: Address,
    ) -> Self {
        AppState {
            host,
            network,
            lightning_client,
            wallet_client,
            bitcoin_client: Arc::new(bitcoin_client),
            address,
        }
    }
}

type SharedState = Arc<Mutex<AppState>>;

const MAX_SEND_AMOUNT: u64 = 10_000_000;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::try_init()?;
    let state = setup().await;

    let app = Router::new()
        .route("/api/onchain", post(onchain_handler))
        .route("/api/lightning", post(lightning_handler))
        .route("/api/bolt11", post(bolt11_handler))
        .route("/api/bip21", get(bip21_handler))
        .route("/api/payjoin", post(payjoin_handler))
        .route("/api/channel", post(channel_handler))
        .fallback(fallback)
        .with_state(state)
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_headers(AllowHeaders::any())
                .allow_methods(AllowMethods::any()),
        );

    // Set up a oneshot channel to handle shutdown signal
    let (tx, rx) = oneshot::channel();

    // Spawn a task to listen for shutdown signals
    tokio::spawn(async move {
        let mut term_signal = signal(SignalKind::terminate())
            .map_err(|e| eprintln!("failed to install TERM signal handler: {e}"))
            .unwrap();
        let mut int_signal = signal(SignalKind::interrupt())
            .map_err(|e| {
                eprintln!("failed to install INT signal handler: {e}");
            })
            .unwrap();

        tokio::select! {
            _ = term_signal.recv() => {
                println!("Received SIGTERM");
            },
            _ = int_signal.recv() => {
                println!("Received SIGINT");
            },
        }

        let _ = tx.send(());
    });

    let addr = SocketAddr::from(([0, 0, 0, 0], 3001));
    println!("listening on {}", addr);

    let server = axum::Server::bind(&addr).serve(app.into_make_service());

    let graceful = server.with_graceful_shutdown(async {
        let _ = rx.await;
    });

    // Await the server to receive the shutdown signal
    if let Err(e) = graceful.await {
        eprintln!("shutdown error: {e}");
    }

    println!("Graceful shutdown complete");

    Ok(())
}

#[axum::debug_handler]
async fn onchain_handler(
    State(state): State<SharedState>,
    Json(payload): Json<OnchainRequest>,
) -> Result<Json<OnchainResponse>, AppError> {
    let txid = pay_onchain(state.clone(), payload.clone()).await?;

    Ok(Json(OnchainResponse { txid }))
}

#[axum::debug_handler]
async fn lightning_handler(
    State(state): State<SharedState>,
    Json(payload): Json<LightningRequest>,
) -> Result<Json<LightningResponse>, AppError> {
    let payment_hash = pay_lightning(state.clone(), payload.clone()).await?;

    Ok(Json(LightningResponse { payment_hash }))
}

#[axum::debug_handler]
async fn bolt11_handler(
    State(state): State<SharedState>,
    Json(payload): Json<Bolt11Request>,
) -> Result<Json<Bolt11Response>, AppError> {
    let bolt11 = request_bolt11(state.clone(), payload.clone()).await?;

    Ok(Json(Bolt11Response { bolt11 }))
}

#[axum::debug_handler]
async fn channel_handler(
    State(state): State<SharedState>,
    Json(payload): Json<ChannelRequest>,
) -> Result<Json<ChannelResponse>, AppError> {
    let txid = open_channel(state.clone(), payload.clone()).await?;

    Ok(Json(ChannelResponse { txid }))
}

#[axum::debug_handler]
async fn bip21_handler(
    State(state): State<SharedState>,
    Query(request): Query<payjoin::Bip21Request>,
) -> Result<Json<payjoin::Bip21Response>, AppError> {
    let bip21 = payjoin::request_bip21(state.clone(), request.amount).await?;

    Ok(Json(payjoin::Bip21Response { bip21 }))
}

#[axum::debug_handler]
async fn payjoin_handler(
    State(state): State<SharedState>,
    headers: HeaderMap,
    params: Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<String, AppError> {
    let params_str = params
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<String>>()
        .join("&");
    let base64 =
        payjoin::payjoin_request(state.clone(), headers, body.to_vec(), params_str).await?;

    Ok(base64)
}

// Make our own error that wraps `anyhow::Error`.
struct AppError(anyhow::Error);

// Tell axum how to convert `AppError` into a response.
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Error: {}", self.0),
        )
            .into_response()
    }
}

// This enables using `?` on functions that return `Result<_, anyhow::Error>` to turn them into
// `Result<_, AppError>`. That way you don't need to do that manually.
impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

async fn fallback(uri: Uri) -> (StatusCode, String) {
    (StatusCode::NOT_FOUND, format!("No route for {}", uri))
}

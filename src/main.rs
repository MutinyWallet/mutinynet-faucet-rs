use std::{net::SocketAddr, sync::Arc};

use axum::extract::Query;
use axum::http::Uri;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use bitcoincore_rpc::Client;
use lnurl::withdraw::WithdrawalResponse;
use lnurl::Tag;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::oneshot;
use tonic_openssl_lnd::LndLightningClient;
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
mod setup;

#[derive(Clone)]
pub struct AppState {
    pub host: String,
    network: bitcoin::Network,
    lightning_client: LndLightningClient,
    bitcoin_client: Arc<Client>,
}

impl AppState {
    pub fn new(
        host: String,
        lightning_client: LndLightningClient,
        bitcoin_client: Client,
        network: bitcoin::Network,
    ) -> Self {
        AppState {
            host,
            network,
            lightning_client,
            bitcoin_client: Arc::new(bitcoin_client),
        }
    }
}

const MAX_SEND_AMOUNT: u64 = 10_000_000;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::try_init()?;
    let state = setup().await;

    let app = Router::new()
        .route("/api/onchain", post(onchain_handler))
        .route("/api/lightning", post(lightning_handler))
        .route("/api/lnurlw", get(lnurlw_handler))
        .route("/api/lnurlw/callback", get(lnurlw_callback_handler))
        .route("/api/bolt11", post(bolt11_handler))
        .route("/api/channel", post(channel_handler))
        .fallback(fallback)
        .layer(Extension(state))
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
    Extension(state): Extension<AppState>,
    Json(payload): Json<OnchainRequest>,
) -> Result<Json<OnchainResponse>, AppError> {
    let txid = pay_onchain(state, payload.clone()).await?;

    Ok(Json(OnchainResponse { txid }))
}

#[axum::debug_handler]
async fn lightning_handler(
    Extension(state): Extension<AppState>,
    Json(payload): Json<LightningRequest>,
) -> Result<Json<LightningResponse>, AppError> {
    let payment_hash = pay_lightning(state, &payload.bolt11).await?;

    Ok(Json(LightningResponse { payment_hash }))
}

#[axum::debug_handler]
async fn lnurlw_handler() -> Result<Json<WithdrawalResponse>, AppError> {
    let resp = WithdrawalResponse {
        default_description: "Mutinynet Faucet".to_string(),
        callback: "https://faucet.mutinynet.com/api/lnurlw/callback".to_string(),
        k1: "k1".to_string(),
        max_withdrawable: MAX_SEND_AMOUNT * 1_000,
        min_withdrawable: None,
        tag: Tag::WithdrawRequest,
    };

    Ok(Json(resp))
}

#[derive(Deserialize)]
pub struct LnurlWithdrawParams {
    k1: String,
    pr: String,
}

#[axum::debug_handler]
async fn lnurlw_callback_handler(
    Extension(state): Extension<AppState>,
    Query(payload): Query<LnurlWithdrawParams>,
) -> Result<Json<Value>, Json<Value>> {
    if payload.k1 == "k1" {
        pay_lightning(state, &payload.pr)
            .await
            .map_err(|e| Json(json!({"status": "ERROR", "reason": format!("{e}")})))?;
        Ok(Json(json!({"status": "OK"})))
    } else {
        Err(Json(json!({"status": "ERROR", "reason": "Incorrect k1"})))
    }
}

#[axum::debug_handler]
async fn bolt11_handler(
    Extension(state): Extension<AppState>,
    Json(payload): Json<Bolt11Request>,
) -> Result<Json<Bolt11Response>, AppError> {
    let bolt11 = request_bolt11(state, payload.clone()).await?;

    Ok(Json(Bolt11Response { bolt11 }))
}

#[axum::debug_handler]
async fn channel_handler(
    Extension(state): Extension<AppState>,
    Json(payload): Json<ChannelRequest>,
) -> Result<Json<ChannelResponse>, AppError> {
    let txid = open_channel(state, payload.clone()).await?;

    Ok(Json(ChannelResponse { txid }))
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

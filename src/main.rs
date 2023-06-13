use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use bitcoin::{Amount, Address};
use bitcoincore_rpc::{
    Auth, Client, RpcApi,
};
use lightning_invoice::Invoice;
use serde::{Deserialize, Serialize};
use tokio::task;
use std::{env, str::FromStr};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tonic_lnd::{LightningClient};

mod onchain;
mod lightning;
mod setup;

use setup::setup;

use onchain::pay_onchain;
use lightning::pay_lightning;

use crate::{onchain::{OnchainRequest, OnchainResponse}, lightning::{LightningRequest, LightningResponse}};

pub struct AppState {
    network: bitcoin::Network,
    lightning_client: LightningClient,
    bitcoin_client: Arc<Client>,
}

impl AppState {
    pub fn new(lightning_client: LightningClient, bitcoin_client: Client, network: bitcoin::Network) -> Self {
        AppState {
            network,
            lightning_client,
            bitcoin_client: Arc::new(bitcoin_client),
        }
    }
}

type SharedState = Arc<Mutex<AppState>>;

const MAX_SEND_AMOUNT: u64 = 1_000_000;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let state = setup().await;

    let app = Router::new()
        .route("/api/onchain", post(onchain_handler))
        .route("/api/lightning", post(lightning_handler))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!("listening on {}", addr);
    
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await?;

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

// Make our own error that wraps `anyhow::Error`.
struct AppError(anyhow::Error);

// Tell axum how to convert `AppError` into a response.
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("Error: {}", self.0)).into_response()
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

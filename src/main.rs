use axum::{
    extract::State,
    http::{self, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};

use bitcoincore_rpc::Client;

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tonic_openssl_lnd::LndLightningClient;
use tower_http::cors::{Any, CorsLayer};

mod bolt11;
mod lightning;
mod onchain;
mod setup;

use bolt11::{request_bolt11, Bolt11Request, Bolt11Response};
use lightning::{pay_lightning, LightningRequest, LightningResponse};
use onchain::{pay_onchain, OnchainRequest, OnchainResponse};
use setup::setup;

pub struct AppState {
    network: bitcoin::Network,
    lightning_client: LndLightningClient,
    bitcoin_client: Arc<Client>,
}

impl AppState {
    pub fn new(
        lightning_client: LndLightningClient,
        bitcoin_client: Client,
        network: bitcoin::Network,
    ) -> Self {
        AppState {
            network,
            lightning_client,
            bitcoin_client: Arc::new(bitcoin_client),
        }
    }
}

type SharedState = Arc<Mutex<AppState>>;

const MAX_SEND_AMOUNT: u64 = 10_000_000;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let state = setup().await;

    let app = Router::new()
        .route("/api/onchain", post(onchain_handler))
        .route("/api/lightning", post(lightning_handler))
        .route("/api/bolt11", post(bolt11_handler))
        .with_state(state)
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_headers(vec![http::header::CONTENT_TYPE])
                .allow_methods([Method::GET, Method::POST]),
        );

    let addr = SocketAddr::from(([0, 0, 0, 0], 3001));
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

#[axum::debug_handler]
async fn bolt11_handler(
    State(state): State<SharedState>,
    Json(payload): Json<Bolt11Request>,
) -> Result<Json<Bolt11Response>, AppError> {
    let bolt11 = request_bolt11(state.clone(), payload.clone()).await?;

    Ok(Json(Bolt11Response { bolt11 }))
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

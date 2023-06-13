use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::env;
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex, RwLock},
};
use tonic_lnd::{LightningClient, lnrpc::payment};

#[derive(Clone, Deserialize)]
struct OnchainRequest {
    sats: u64,
    address: String,
}

#[derive(Serialize)]
struct OnchainResponse {
    txid: String,
}

#[derive(Clone, Deserialize)]
struct LightningRequest {
    bolt11: String,
}

#[derive(Serialize)]
struct LightningResponse {
    pop: String,
}

struct AppState {
    lightning_client: LightningClient,
}

impl AppState {
    pub fn new(client: LightningClient) -> Self {
        AppState {
            lightning_client: client,
        }
    }
}
// unsafe impl Send for AppState {}

type SharedState = Arc<Mutex<AppState>>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load environment variables from various sources.
    dotenv::from_filename(".env.local").ok();
    dotenv::from_filename(".env").ok();
    dotenv::dotenv().ok();

    let address = env::var("GRPC_HOST").expect("missing GRPC_HOST");
    let macaroon_file = env::var("ADMIN_MACAROON_PATH").expect("missing ADMIN_MACAROON_PATH");
    let cert_file = env::var("TLS_CERT_PATH").expect("missing TLS_CERT_PATH");
    let port: u32 = env::var("GRPC_PORT")
        .expect("missing GRPC_PORT")
        .parse()
        .expect("GRPC_PORT must be a number");

    let client = tonic_lnd::connect(address, port, cert_file, macaroon_file)
        .await
        .expect("failed to connect")
        .lightning()
        .clone();

    // Make sure we can get info at startup
    let _ =client
            .clone().get_info(tonic_lnd::lnrpc::GetInfoRequest {})
            .await
            .expect("failed to get info");

    let state = AppState::new(client);

    let state = Arc::new(Mutex::new(state));

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

async fn onchain_handler(
    Json(payload): Json<OnchainRequest>,
) -> (StatusCode, Json<OnchainResponse>) {
    // let txid = pay_onchain(payload)?;
    (StatusCode::OK, Json(OnchainResponse { txid: "heyo".to_string() }))
}

#[axum::debug_handler]
async fn lightning_handler(
    State(state): State<SharedState>,
    Json(payload): Json<LightningRequest>,
) -> Result<Json<LightningResponse>, AppError> {
    let pop = pay_lightning(state.clone(), payload.clone()).await?;

    Ok(Json(LightningResponse {
        pop,
    }))

    // (StatusCode::OK, Json(LightningResponse {
    //     pop: "abc123".to_string(),
    // }))
}

async fn pay_lightning(state: Arc<Mutex<AppState>>, payload: LightningRequest) -> anyhow::Result<String> {
    let payment_hash = {
        let mut lightning_client = state
            .clone()
            .lock()
            .map_err(|_| anyhow::anyhow!("failed to get lock"))?
            .lightning_client
            .clone();


        let response = lightning_client.send_payment_sync(tonic_lnd::lnrpc::SendRequest {
            payment_request: payload.bolt11,
            ..Default::default()
        }).await?.into_inner();

        // dbg!(response.clone());

        if response.payment_error != "" {
            return Err(anyhow::anyhow!("Payment error: {}", response.payment_error));
        }

        response.payment_hash
    };

    let hex_payment_hash = hex::encode(payment_hash.clone());
   
    Ok(hex_payment_hash)
}

// Make our own error that wraps `anyhow::Error`.
struct AppError(anyhow::Error);

// Tell axum how to convert `AppError` into a response.
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("{}", self.0),
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
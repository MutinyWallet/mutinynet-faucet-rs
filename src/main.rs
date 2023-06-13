use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{post},
    Router, Json,
};
use serde::{Serialize, Deserialize};
use std::net::SocketAddr;

#[derive(Deserialize)]
struct OnchainRequest {
    sats: u64,
    address: String,
}

#[derive(Serialize)]
struct OnchainResponse {
    txid: String,
}

#[derive(Deserialize)]
struct LightningRequest {
    bolt11: String,
}

#[derive(Serialize)]
struct LightningResponse {
    pop: String,
}

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/api/onchain", post(onchain_handler))
        .route("/api/lightning", post(lightning_handler));

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

async fn onchain_handler(Json(payload): Json<OnchainRequest>) -> Result<Json<OnchainResponse>, AppError> {
    let txid = pay_onchain(payload)?;
    Ok(Json(OnchainResponse { txid }))
}


async fn lightning_handler(Json(payload): Json<LightningRequest>) -> Result<Json<LightningResponse>, AppError> {
    let pop = pay_lightning(payload)?;
    Ok(Json(LightningResponse { pop}))
}

fn pay_onchain(req: OnchainRequest) -> Result<String, anyhow::Error> {
    anyhow::bail!("it failed!")
    // Ok("abc123".to_string())
}

fn pay_lightning(req: LightningRequest) -> Result<String, anyhow::Error> {
    // anyhow::bail!("it failed!")
    Ok("abc123".to_string())
}

// Make our own error that wraps `anyhow::Error`.
struct AppError(anyhow::Error);

// Tell axum how to convert `AppError` into a response.
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Something went wrong: {}", self.0),
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
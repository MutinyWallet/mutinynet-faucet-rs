use axum::extract::Query;
use axum::headers::{HeaderMap, HeaderValue};
use axum::http::Uri;
use axum::response::Redirect;
use axum::{
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use bitcoin_waila::PaymentParams;
use jsonwebtoken::{encode, EncodingKey, Header};
use lnurl::withdraw::WithdrawalResponse;
use lnurl::{AsyncClient, Tag};
use log::error;
use nostr::key::Keys;
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::str::FromStr;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::oneshot;
use tonic_openssl_lnd::LndLightningClient;
use tower_http::cors::{AllowMethods, Any, CorsLayer};

use crate::auth::{auth_middleware, AuthState, AuthUser, GithubCallback};
use crate::nostr_dms::listen_to_nostr_dms;
use crate::payments::PaymentsByIp;
use bolt11::{request_bolt11, Bolt11Request, Bolt11Response};
use channel::{open_channel, ChannelRequest, ChannelResponse};
use lightning::{pay_lightning, LightningRequest, LightningResponse};
use onchain::{pay_onchain, OnchainRequest, OnchainResponse};
use setup::setup;

mod auth;
mod bolt11;
mod channel;
mod lightning;
mod nostr_dms;
mod onchain;
mod payments;
mod setup;

#[derive(Clone)]
pub struct AppState {
    pub host: String,
    keys: Keys,
    network: bitcoin::Network,
    lightning_client: LndLightningClient,
    lnurl: AsyncClient,
    payments: PaymentsByIp,
    auth: AuthState,
}

impl AppState {
    pub fn new(
        host: String,
        keys: Keys,
        lightning_client: LndLightningClient,
        network: bitcoin::Network,
        auth: AuthState,
    ) -> Self {
        let lnurl = lnurl::Builder::default().build_async().unwrap();
        AppState {
            host,
            keys,
            network,
            lightning_client,
            lnurl,
            payments: PaymentsByIp::new(),
            auth,
        }
    }
}

const MAX_SEND_AMOUNT: u64 = 1_000_000;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let state = setup().await?;

    let app: Router = Router::new()
        .route("/auth/github", get(github_auth))
        .route("/auth/github/callback", get(github_callback))
        .route(
            "/api/onchain",
            post(onchain_handler).route_layer(middleware::from_fn(auth_middleware)),
        )
        .route("/api/lightning", post(lightning_handler))
        .route("/api/lnurlw", get(lnurlw_handler))
        .route("/api/lnurlw/callback", get(lnurlw_callback_handler))
        .route("/api/bolt11", post(bolt11_handler))
        .route("/api/channel", post(channel_handler))
        .fallback(fallback)
        .layer(Extension(state.clone()))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_headers([axum::http::header::AUTHORIZATION])
                .allow_methods(AllowMethods::any()),
        );

    // start dm listener thread
    tokio::spawn(async move {
        loop {
            if let Err(e) = listen_to_nostr_dms(state.clone()).await {
                error!("Error listening to nostr dms: {e}");
            }
        }
    });

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
async fn github_auth(Extension(state): Extension<AppState>) -> Result<Redirect, AppError> {
    let redirect_url = format!(
        "https://github.com/login/oauth/authorize?client_id={}&scope=user:email&redirect_uri={}/auth/github/callback",
        state.auth.github_client_id,
        state.host
    );
    Ok(Redirect::temporary(&redirect_url))
}

#[derive(Deserialize)]
struct GithubEmail {
    email: String,
    primary: bool,
    verified: bool,
}

#[axum::debug_handler]
async fn github_callback(
    Query(params): Query<GithubCallback>,
    Extension(state): Extension<AppState>,
) -> Result<Redirect, StatusCode> {
    // Exchange code for access token
    let token_response = state
        .auth
        .client
        .post("https://github.com/login/oauth/access_token")
        .header("Accept", "application/json")
        .json(&json!({
            "client_id": state.auth.github_client_id,
            "client_secret": state.auth.github_client_secret,
            "code": params.code,
        }))
        .send()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .json::<auth::GithubTokenResponse>()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Get user info
    // Get user's email
    let user_emails = state
        .auth
        .client
        .get("https://api.github.com/user/emails")
        .header(
            "Authorization",
            format!("Bearer {}", token_response.access_token),
        )
        .header("User-Agent", "rust-github-oauth")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .json::<Vec<GithubEmail>>()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Find primary email
    let primary_email = user_emails
        .into_iter()
        .find(|email| email.primary && email.verified)
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    // Create JWT
    let claims = auth::TokenClaims {
        sub: primary_email.email,
        exp: (chrono::Utc::now() + chrono::Duration::hours(24)).timestamp() as usize,
        iat: chrono::Utc::now().timestamp() as usize,
    };

    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(state.auth.jwt_secret.as_bytes()),
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Redirect to frontend with token
    Ok(Redirect::temporary(&format!(
        "{}/?token={token}",
        state.host
    )))
}

#[axum::debug_handler]
async fn onchain_handler(
    Extension(state): Extension<AppState>,
    Extension(user): Extension<AuthUser>,
    headers: HeaderMap,
    Json(payload): Json<OnchainRequest>,
) -> Result<Json<OnchainResponse>, AppError> {
    // Extract the X-Forwarded-For header
    let x_forwarded_for = headers
        .get("x-forwarded-for")
        .and_then(|x| HeaderValue::to_str(x).ok())
        .unwrap_or("Unknown");

    let params = PaymentParams::from_str(&payload.address)
        .map_err(|_| anyhow::anyhow!("invalid address"))?;
    let address_str = params.address().ok_or(anyhow::anyhow!("invalid address"))?;

    if state
        .payments
        .verify_payments(x_forwarded_for, Some(&address_str), Some(&user))
        .await
    {
        return Err(AppError::new("Too many payments"));
    }

    let res = pay_onchain(&state, x_forwarded_for, user, payload).await?;

    Ok(Json(res))
}

#[axum::debug_handler]
async fn lightning_handler(
    Extension(state): Extension<AppState>,
    headers: HeaderMap,
    Json(payload): Json<LightningRequest>,
) -> Result<Json<LightningResponse>, AppError> {
    // Extract the X-Forwarded-For header
    let x_forwarded_for = headers
        .get("x-forwarded-for")
        .and_then(|x| HeaderValue::to_str(x).ok())
        .unwrap_or("Unknown");

    if state.payments.get_total_payments(x_forwarded_for).await > MAX_SEND_AMOUNT * 10 {
        return Err(AppError::new("Too many payments"));
    }

    let payment_hash = pay_lightning(&state, x_forwarded_for, &payload.bolt11).await?;

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
    headers: HeaderMap,
    Query(payload): Query<LnurlWithdrawParams>,
) -> Result<Json<Value>, Json<Value>> {
    if payload.k1 == "k1" {
        // Extract the X-Forwarded-For header
        let x_forwarded_for = headers
            .get("x-forwarded-for")
            .and_then(|x| HeaderValue::to_str(x).ok())
            .unwrap_or("Unknown");

        if state.payments.get_total_payments(x_forwarded_for).await > MAX_SEND_AMOUNT * 10 {
            return Err(Json(json!({"status": "ERROR", "reason": "Incorrect k1"})));
        }

        pay_lightning(&state, x_forwarded_for, &payload.pr)
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
    let bolt11 = request_bolt11(&state, payload.clone()).await?;

    Ok(Json(Bolt11Response { bolt11 }))
}

#[axum::debug_handler]
async fn channel_handler(
    Extension(state): Extension<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ChannelRequest>,
) -> Result<Json<ChannelResponse>, AppError> {
    // Extract the X-Forwarded-For header
    let x_forwarded_for = headers
        .get("x-forwarded-for")
        .and_then(|x| HeaderValue::to_str(x).ok())
        .unwrap_or("Unknown");

    if state.payments.get_total_payments(x_forwarded_for).await > MAX_SEND_AMOUNT * 10 {
        return Err(AppError::new("Too many payments"));
    }

    let txid = open_channel(&state, x_forwarded_for, payload).await?;

    Ok(Json(ChannelResponse { txid }))
}

// Make our own error that wraps `anyhow::Error`.
struct AppError(anyhow::Error);

impl AppError {
    fn new(msg: &'static str) -> Self {
        AppError(anyhow::anyhow!(msg))
    }
}

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

use axum::extract::Query;
use axum::headers::{HeaderMap, HeaderValue};
use axum::http::{Request, Uri};
use axum::middleware::Next;
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
use log::{error, info, warn};
use nostr::key::Keys;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, oneshot};
use tonic_openssl_lnd::LndLightningClient;
use tower_http::cors::{AllowMethods, Any, CorsLayer};

use crate::analytics::{
    analytics_domains, analytics_l402, analytics_recent, analytics_summary, analytics_timeseries,
    analytics_users,
};
use crate::auth::{auth_middleware, is_premium, AuthState, AuthUser, GithubCallback};
use crate::nostr_dms::listen_to_nostr_dms;
use crate::payments::PaymentsByIp;
use bolt11::{request_bolt11, Bolt11Request, Bolt11Response};
use channel::{open_channel, ChannelRequest, ChannelResponse};
use l402::{generate_l402_token, L402Config};
use lightning::{pay_lightning, LightningRequest, LightningResponse};
use onchain::{pay_onchain, OnchainRequest, OnchainResponse};
use reorg::{
    generate_reorg_invoice, start_reorg_invoice_listener, ReorgInvoiceRequest, ReorgInvoiceResponse,
};
use setup::setup;

mod analytics;
mod auth;
mod bolt11;
mod channel;
mod l402;
mod lightning;
mod nostr_dms;
mod onchain;
mod payments;
mod reorg;
mod setup;

#[derive(Clone)]
pub struct AppState {
    pub host: String,
    keys: Keys,
    network: bitcoin::Network,
    lightning_client: LndLightningClient,
    mainnet_lightning_client: Option<LndLightningClient>,
    bitcoin_rpc: Option<Arc<bitcoincore_rpc::Client>>,
    reorg_db: Option<SqlitePool>,
    lnurl: AsyncClient,
    payments: PaymentsByIp,
    auth: AuthState,
    reorg_config: ReorgConfig,
    l402_config: L402Config,
    /// Pool for read queries (dashboard endpoints)
    pub analytics_db: Option<SqlitePool>,
    /// Batched writer channel for recording payments
    pub analytics_writer: Option<mpsc::UnboundedSender<analytics::AnalyticsPayment>>,
    /// API token for analytics endpoints
    pub analytics_token: Option<String>,
}

#[derive(Clone)]
pub struct ReorgConfig {
    enabled: bool,
    cooldown_seconds: u64,
    pricing: HashMap<u8, u64>,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        host: String,
        keys: Keys,
        lightning_client: LndLightningClient,
        mainnet_lightning_client: Option<LndLightningClient>,
        bitcoin_rpc: Option<Arc<bitcoincore_rpc::Client>>,
        reorg_db: Option<SqlitePool>,
        network: bitcoin::Network,
        auth: AuthState,
        reorg_config: ReorgConfig,
        l402_config: L402Config,
        analytics_db: Option<SqlitePool>,
        analytics_writer: Option<mpsc::UnboundedSender<analytics::AnalyticsPayment>>,
        analytics_token: Option<String>,
    ) -> Self {
        let lnurl = lnurl::Builder::default().build_async().unwrap();
        AppState {
            host,
            keys,
            network,
            lightning_client,
            mainnet_lightning_client,
            bitcoin_rpc,
            reorg_db,
            lnurl,
            payments: PaymentsByIp::new(),
            auth,
            reorg_config,
            l402_config,
            analytics_db,
            analytics_writer,
            analytics_token,
        }
    }
}

const MAX_SEND_AMOUNT: u64 = 1_000_000;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let state = setup().await?;

    let app: Router = Router::new()
        .route("/auth/github/client_id", get(github_client_id))
        .route("/auth/github", get(github_auth))
        .route("/auth/github/callback", get(github_callback))
        .route("/auth/github/device", post(github_device))
        .route(
            "/auth/check",
            get(auth_check).route_layer(middleware::from_fn(auth_middleware)),
        )
        .route(
            "/api/onchain",
            post(onchain_handler).route_layer(middleware::from_fn(auth_middleware)),
        )
        .route(
            "/api/lightning",
            post(lightning_handler).route_layer(middleware::from_fn(auth_middleware)),
        )
        .route("/api/lnurlw", get(lnurlw_handler))
        .route("/api/lnurlw/callback", get(lnurlw_callback_handler))
        .route("/api/bolt11", post(bolt11_handler))
        .route("/api/l402", post(l402_handler).get(l402_challenge_handler))
        .route("/api/l402/check", get(l402_check_handler))
        .route(
            "/api/channel",
            post(channel_handler).route_layer(middleware::from_fn(auth_middleware)),
        )
        .route(
            "/api/reorg/invoice",
            post(reorg_invoice_handler).route_layer(middleware::from_fn(auth_middleware)),
        )
        .route(
            "/api/analytics/summary",
            get(analytics_summary).route_layer(middleware::from_fn(analytics_auth_middleware)),
        )
        .route(
            "/api/analytics/timeseries",
            get(analytics_timeseries).route_layer(middleware::from_fn(analytics_auth_middleware)),
        )
        .route(
            "/api/analytics/users",
            get(analytics_users).route_layer(middleware::from_fn(analytics_auth_middleware)),
        )
        .route(
            "/api/analytics/recent",
            get(analytics_recent).route_layer(middleware::from_fn(analytics_auth_middleware)),
        )
        .route(
            "/api/analytics/domains",
            get(analytics_domains).route_layer(middleware::from_fn(analytics_auth_middleware)),
        )
        .route(
            "/api/analytics/l402",
            get(analytics_l402).route_layer(middleware::from_fn(analytics_auth_middleware)),
        )
        .fallback(fallback)
        .layer(Extension(state.clone()))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_headers([axum::http::header::AUTHORIZATION])
                .allow_methods(AllowMethods::any()),
        );

    // start dm listener thread
    let dm_state = state.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = listen_to_nostr_dms(dm_state.clone()).await {
                error!("Error listening to nostr dms: {e}");
            }
        }
    });

    // start reorg invoice listener thread
    if state.reorg_config.enabled {
        let reorg_state = state.clone();
        tokio::spawn(async move {
            start_reorg_invoice_listener(reorg_state).await;
        });
    }

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
async fn github_client_id(Extension(state): Extension<AppState>) -> Json<Value> {
    Json(json!({ "client_id": state.auth.github_client_id }))
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

#[derive(Serialize)]
struct DeviceReturn {
    token: String,
}

#[axum::debug_handler]
async fn github_device(
    Extension(state): Extension<AppState>,
    Json(params): Json<GithubCallback>,
) -> Result<Json<DeviceReturn>, StatusCode> {
    // Get user info
    // Get user's email
    let user_emails = state
        .auth
        .client
        .get("https://api.github.com/user/emails")
        .header("Authorization", format!("Bearer {}", params.code))
        .header("User-Agent", "rust-github-oauth")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .json::<Vec<GithubEmail>>()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Find primary email
    let primary_email: GithubEmail = user_emails
        .into_iter()
        .find(|email| email.primary && email.verified)
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    // Check if user is banned
    if auth::is_banned(&primary_email.email) {
        warn!("User {} is banned!", primary_email.email);
        return Err(StatusCode::BAD_REQUEST);
    }

    info!(
        "Authing user with email through device: {}",
        primary_email.email
    );

    // Create JWT
    let claims = auth::TokenClaims {
        sub: primary_email.email,
        exp: (chrono::Utc::now() + chrono::Duration::days(31)).timestamp() as usize,
        iat: chrono::Utc::now().timestamp() as usize,
    };

    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(state.auth.jwt_secret.as_bytes()),
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Redirect to frontend with token
    Ok(Json(DeviceReturn { token }))
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
    let primary_email: GithubEmail = user_emails
        .into_iter()
        .find(|email| email.primary && email.verified)
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    // Check if user is banned
    if auth::is_banned(&primary_email.email) {
        warn!("User {} is banned!", primary_email.email);
        return Err(StatusCode::BAD_REQUEST);
    }

    info!("Authing user with email: {}", primary_email.email);

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
async fn auth_check(
    Extension(_state): Extension<AppState>,
    Extension(_user): Extension<AuthUser>,
) -> Result<Json<Value>, AppError> {
    Ok(Json(json!({"status": "OK"})))
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
        && !is_premium(&user.username)
    {
        return Err(AppError::new("Too many payments"));
    }

    let res = pay_onchain(&state, x_forwarded_for, user, payload).await?;

    Ok(Json(res))
}

#[axum::debug_handler]
async fn lightning_handler(
    Extension(state): Extension<AppState>,
    Extension(user): Extension<AuthUser>,
    headers: HeaderMap,
    Json(payload): Json<LightningRequest>,
) -> Result<Json<LightningResponse>, AppError> {
    // Extract the X-Forwarded-For header
    let x_forwarded_for = headers
        .get("x-forwarded-for")
        .and_then(|x| HeaderValue::to_str(x).ok())
        .unwrap_or("Unknown");

    if state
        .payments
        .verify_payments(x_forwarded_for, None, Some(&user))
        .await
        && !is_premium(&user.username)
    {
        return Err(AppError::new("Too many payments"));
    }

    let payment_hash = pay_lightning(&state, x_forwarded_for, Some(&user), &payload.bolt11).await?;

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

        if state.payments.get_total_payments(x_forwarded_for).await > MAX_SEND_AMOUNT {
            return Err(Json(json!({"status": "ERROR", "reason": "Incorrect k1"})));
        }

        pay_lightning(&state, x_forwarded_for, None, &payload.pr)
            .await
            .map_err(|e| Json(json!({"status": "ERROR", "reason": format!("{e}")})))?;
        Ok(Json(json!({"status": "OK"})))
    } else {
        Err(Json(json!({"status": "ERROR", "reason": "Incorrect k1"})))
    }
}

#[derive(Serialize)]
struct L402HandlerResponse {
    invoice: String,
    token: String,
}

async fn generate_l402_challenge(state: &AppState) -> Result<L402HandlerResponse, AppError> {
    if !state.l402_config.enabled {
        return Err(AppError::new("L402 authentication is not enabled"));
    }

    let mainnet_client = state
        .mainnet_lightning_client
        .as_ref()
        .ok_or_else(|| AppError::new("Mainnet LND not configured"))?;

    let response = generate_l402_token(
        mainnet_client,
        &state.auth.jwt_secret,
        state.l402_config.invoice_amount_sats,
    )
    .await?;

    if let Some(tx) = &state.analytics_writer {
        analytics::record_payment(
            tx,
            "l402_issued",
            state.l402_config.invoice_amount_sats,
            None,
            "n/a",
            Some(&response.invoice),
        );
    }

    Ok(L402HandlerResponse {
        invoice: response.invoice,
        token: response.token,
    })
}

/// GET /api/l402 — returns 402 Payment Required with WWW-Authenticate header
/// for spec-compliant L402 discovery (e.g. 402index.io)
#[axum::debug_handler]
async fn l402_challenge_handler(
    Extension(state): Extension<AppState>,
) -> Result<Response, AppError> {
    let challenge = generate_l402_challenge(&state).await?;

    let www_auth = format!(
        "L402 token=\"{}\", invoice=\"{}\"",
        challenge.token, challenge.invoice
    );

    Ok((
        StatusCode::PAYMENT_REQUIRED,
        [(axum::http::header::WWW_AUTHENTICATE, www_auth)],
        Json(json!({
            "invoice": challenge.invoice,
            "token": challenge.token,
        })),
    )
        .into_response())
}

#[axum::debug_handler]
async fn l402_handler(
    Extension(state): Extension<AppState>,
) -> Result<Json<L402HandlerResponse>, AppError> {
    let challenge = generate_l402_challenge(&state).await?;
    Ok(Json(challenge))
}

#[derive(Deserialize)]
struct L402CheckParams {
    token: String,
}

#[axum::debug_handler]
async fn l402_check_handler(
    Extension(state): Extension<AppState>,
    Query(params): Query<L402CheckParams>,
) -> Result<Json<Value>, AppError> {
    if !state.l402_config.enabled {
        return Err(AppError::new("L402 authentication is not enabled"));
    }

    let mainnet_client = state
        .mainnet_lightning_client
        .as_ref()
        .ok_or_else(|| AppError::new("Mainnet LND not configured"))?;

    // Decode the JWT to get the payment_hash
    let token_data = jsonwebtoken::decode::<l402::L402Claims>(
        &params.token,
        &jsonwebtoken::DecodingKey::from_secret(state.auth.jwt_secret.as_bytes()),
        &jsonwebtoken::Validation::default(),
    )
    .map_err(|_| AppError::new("Invalid token"))?;

    let payment_hash_hex = &token_data.claims.payment_hash;
    let payment_hash_bytes =
        hex::decode(payment_hash_hex).map_err(|_| AppError::new("Invalid payment hash"))?;

    let lookup_request = tonic_openssl_lnd::lnrpc::PaymentHash {
        r_hash: payment_hash_bytes,
        ..Default::default()
    };

    let invoice = mainnet_client
        .clone()
        .lookup_invoice(lookup_request)
        .await
        .map_err(|_| AppError::new("Failed to lookup invoice"))?
        .into_inner();

    if invoice.state == tonic_openssl_lnd::lnrpc::invoice::InvoiceState::Settled as i32 {
        let preimage_hex = hex::encode(&invoice.r_preimage);
        Ok(Json(json!({
            "status": "settled",
            "preimage": preimage_hex,
        })))
    } else if invoice.state == tonic_openssl_lnd::lnrpc::invoice::InvoiceState::Canceled as i32 {
        Ok(Json(json!({
            "status": "expired",
        })))
    } else {
        Ok(Json(json!({
            "status": "pending",
        })))
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
    Extension(user): Extension<AuthUser>,
    headers: HeaderMap,
    Json(payload): Json<ChannelRequest>,
) -> Result<Json<ChannelResponse>, AppError> {
    // Extract the X-Forwarded-For header
    let x_forwarded_for = headers
        .get("x-forwarded-for")
        .and_then(|x| HeaderValue::to_str(x).ok())
        .unwrap_or("Unknown");

    if state
        .payments
        .verify_payments(x_forwarded_for, None, Some(&user))
        .await
        && !is_premium(&user.username)
    {
        return Err(AppError::new("Too many payments"));
    }

    let txid = open_channel(&state, x_forwarded_for, Some(&user), payload).await?;

    Ok(Json(ChannelResponse { txid }))
}

#[axum::debug_handler]
async fn reorg_invoice_handler(
    Extension(state): Extension<AppState>,
    Extension(user): Extension<AuthUser>,
    Json(payload): Json<ReorgInvoiceRequest>,
) -> Result<Json<ReorgInvoiceResponse>, AppError> {
    let response = generate_reorg_invoice(&state, &user, payload).await?;
    Ok(Json(response))
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

async fn analytics_auth_middleware<B>(
    headers: HeaderMap,
    request: Request<B>,
    next: Next<B>,
) -> Result<Response, StatusCode> {
    let state = request
        .extensions()
        .get::<AppState>()
        .expect("AppState not found in extensions");

    let token = match &state.analytics_token {
        Some(t) => t,
        None => return Err(StatusCode::NOT_FOUND),
    };

    let provided = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;

    if provided != token {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(request).await)
}

async fn fallback(uri: Uri) -> (StatusCode, String) {
    (StatusCode::NOT_FOUND, format!("No route for {}", uri))
}

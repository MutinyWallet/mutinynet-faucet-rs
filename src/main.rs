use axum::extract::{ConnectInfo, Query};
use axum::headers::{HeaderMap, HeaderValue};
use axum::http::Request;
use axum::middleware::Next;
use axum::response::Redirect;
use axum::{
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use jsonwebtoken::{encode, EncodingKey, Header};
use ldk_server_client::client::LdkServerClient;
use ldk_server_client::ldk_server_grpc::api::GetPaymentDetailsRequest;
use ldk_server_client::ldk_server_grpc::types::{payment_kind, PaymentStatus};
use lightning_invoice::Bolt11Invoice;
use lnurl::withdraw::WithdrawalResponse;
use lnurl::Tag;
use log::{error, info, warn};
use nostr::key::Keys;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{oneshot, Mutex};
use tonic_openssl_lnd::{LndLightningClient, LndRouterClient};
use tower_http::cors::{AllowMethods, CorsLayer};

use crate::admin::{admin_add, admin_list, admin_remove};
use crate::analytics::{
    analytics_balance, analytics_combined, analytics_domains, analytics_l402, analytics_recent,
    analytics_summary, analytics_timeseries, analytics_users, user_recent, AnalyticsWriter,
};
use crate::arkade::{dispense_arkade, ArkadeError, ArkadeRequest, ArkadeResponse};
use crate::auth::{auth_middleware, AuthState, AuthUser, GithubCallback, UsersCache};
use crate::monitoring::{monitoring_health_handler, MonitoringHealth};
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

mod admin;
mod analytics;
mod arkade;
mod auth;
mod bolt11;
mod channel;
mod l402;
mod lightning;
mod monitoring;
mod nostr_dms;
mod onchain;
mod payment_instructions;
mod payments;
mod reorg;
mod setup;

#[derive(Clone)]
pub struct AppState {
    pub host: String,
    keys: Keys,
    network: bitcoin::Network,
    lightning_client: LndLightningClient,
    router_client: LndRouterClient,
    mainnet_ldk_client: Option<LdkServerClient>,
    bitcoin_rpc: Option<Arc<bitcoincore_rpc::Client>>,
    reorg_db: Option<SqlitePool>,
    /// Serializes reorg invoice creation and execution so the database checks
    /// and their external side effects cannot race within this process.
    reorg_operation_lock: Arc<Mutex<()>>,
    payments: PaymentsByIp,
    auth: AuthState,
    reorg_config: ReorgConfig,
    l402_config: L402Config,
    /// User management database (banned/premium/whitelisted users and domains)
    pub users_db: SqlitePool,
    /// In-memory cache for user lists (ban/premium checks)
    pub users_cache: Arc<UsersCache>,
    /// API token for admin endpoints
    pub admin_token: Option<String>,
    /// Pool for read queries (dashboard endpoints)
    pub analytics_db: Option<SqlitePool>,
    /// Batched writer channel for recording payments
    pub analytics_writer: Option<AnalyticsWriter>,
    /// API token for analytics endpoints
    pub analytics_token: Option<String>,
    /// Health state for analytics storage and Telegram alerts.
    pub monitoring_health: MonitoringHealth,
    /// Base URL of the Arkade dispenser daemon on the internal network.
    /// If unset, POST /api/arkade returns an error.
    pub arkade_daemon_url: Option<String>,
    /// Optional shared secret sent to the daemon as X-Internal-Token.
    pub arkade_internal_token: Option<String>,
    /// The container's default gateway, trusted as a proxy peer alongside
    /// loopback (docker-proxy hairpins host traffic, so nginx arrives with
    /// the gateway IP as its peer). None outside containers: loopback only.
    pub trusted_gateway: Option<IpAddr>,
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
        router_client: LndRouterClient,
        mainnet_ldk_client: Option<LdkServerClient>,
        bitcoin_rpc: Option<Arc<bitcoincore_rpc::Client>>,
        reorg_db: Option<SqlitePool>,
        network: bitcoin::Network,
        auth: AuthState,
        reorg_config: ReorgConfig,
        l402_config: L402Config,
        users_db: SqlitePool,
        users_cache: Arc<UsersCache>,
        admin_token: Option<String>,
        analytics_db: Option<SqlitePool>,
        analytics_writer: Option<AnalyticsWriter>,
        analytics_token: Option<String>,
        monitoring_health: MonitoringHealth,
        arkade_daemon_url: Option<String>,
        arkade_internal_token: Option<String>,
        trusted_gateway: Option<IpAddr>,
    ) -> Self {
        AppState {
            host,
            keys,
            network,
            lightning_client,
            router_client,
            mainnet_ldk_client,
            bitcoin_rpc,
            reorg_db,
            reorg_operation_lock: Arc::new(Mutex::new(())),
            payments: PaymentsByIp::new(),
            auth,
            reorg_config,
            l402_config,
            users_db,
            users_cache,
            admin_token,
            analytics_db,
            analytics_writer,
            analytics_token,
            monitoring_health,
            arkade_daemon_url,
            arkade_internal_token,
            trusted_gateway,
        }
    }
}

const MAX_SEND_AMOUNT: u64 = 1_000_000;

/// Daily per-IP limit for unauthenticated invoice creation endpoints.
const INVOICE_REQ_DAILY_LIMIT: u64 = 60;

/// Daily per-IP limit for L402 status checks (clients poll this endpoint).
const L402_CHECK_DAILY_LIMIT: u64 = 600;

/// How long a challenge (LNURL-withdraw k1, OAuth state) stays valid.
const CHALLENGE_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// Hard cap for outstanding LNURL-withdraw challenges in persistent storage.
const MAX_OUTSTANDING_LNURLW_CHALLENGES: i64 = 10_000;

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
            "/api/arkade",
            post(arkade_handler).route_layer(middleware::from_fn(auth_middleware)),
        )
        .route(
            "/api/reorg/invoice",
            post(reorg_invoice_handler).route_layer(middleware::from_fn(auth_middleware)),
        )
        .route(
            "/api/recent",
            get(user_recent).route_layer(middleware::from_fn(auth_middleware)),
        )
        .route(
            "/api/limits",
            get(limits_handler).route_layer(middleware::from_fn(auth_middleware)),
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
        .route(
            "/api/analytics/balance",
            get(analytics_balance).route_layer(middleware::from_fn(analytics_auth_middleware)),
        )
        .route(
            "/api/analytics/monitoring/health",
            get(monitoring_health_handler)
                .route_layer(middleware::from_fn(analytics_auth_middleware)),
        )
        .route(
            "/api/analytics",
            get(analytics_combined).route_layer(middleware::from_fn(analytics_auth_middleware)),
        )
        .route(
            "/api/admin/:list",
            get(admin_list)
                .post(admin_add)
                .delete(admin_remove)
                .route_layer(middleware::from_fn(admin_auth_middleware)),
        )
        .fallback(fallback)
        .layer(Extension(state.clone()))
        .layer(
            // Only the configured frontend origin may make credentialed
            // cross-origin requests; Any would let any website use a
            // leaked JWT from a browser.
            CorsLayer::new()
                .allow_origin(
                    state
                        .host
                        .trim_end_matches('/')
                        .parse::<axum::http::HeaderValue>()
                        .expect("HOST must be a valid origin URL"),
                )
                .allow_headers([
                    axum::http::header::AUTHORIZATION,
                    axum::http::HeaderName::from_static(L402_CLAIM_HEADER),
                ])
                .allow_methods(AllowMethods::any()),
        );

    // periodically prune empty rate-limit trackers so the map stays bounded
    {
        let payments = state.payments.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(600)).await;
                payments.prune().await;
            }
        });
    }

    // start dm listener thread
    let dm_state = state.clone();
    tokio::spawn(async move {
        let mut backoff = std::time::Duration::from_secs(1);
        loop {
            if let Err(e) = listen_to_nostr_dms(dm_state.clone()).await {
                error!("Error listening to nostr dms: {e}");
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(std::time::Duration::from_secs(300));
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

    let server =
        axum::Server::bind(&addr).serve(app.into_make_service_with_connect_info::<SocketAddr>());

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

const OAUTH_STATE_COOKIE: &str = "mutinynet_oauth_state";

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|cookies| {
            cookies.split(';').find_map(|cookie| {
                let (cookie_name, value) = cookie.trim().split_once('=')?;
                (cookie_name == name).then_some(value)
            })
        })
}

fn oauth_state_cookie(value: &str, secure: bool, max_age: u64) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{OAUTH_STATE_COOKIE}={value}; Path=/auth/github/callback; Max-Age={max_age}; HttpOnly; SameSite=Lax{secure}"
    )
}

#[derive(Debug, PartialEq, Eq)]
enum GithubCallbackError {
    MissingState,
    MissingCookie,
    StateMismatch,
    Banned,
    Internal,
}

impl IntoResponse for GithubCallbackError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::MissingState => (
                StatusCode::BAD_REQUEST,
                "GitHub sign-in state is missing. Return to the faucet and sign in again.",
            ),
            Self::MissingCookie => (
                StatusCode::BAD_REQUEST,
                "GitHub sign-in cookie is missing or expired. Allow cookies for the faucet, then return to the faucet and sign in again.",
            ),
            Self::StateMismatch => (
                StatusCode::BAD_REQUEST,
                "GitHub sign-in state does not match. Return to the faucet and sign in again in one browser tab.",
            ),
            Self::Banned => (
                StatusCode::FORBIDDEN,
                "This GitHub account or email domain is not allowed to use the faucet.",
            ),
            Self::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "GitHub sign-in could not be completed. Return to the faucet and try again.",
            ),
        };
        (status, message).into_response()
    }
}

fn validate_oauth_state(
    state: Option<&str>,
    headers: &HeaderMap,
) -> Result<(), GithubCallbackError> {
    let state = state
        .filter(|state| !state.is_empty())
        .ok_or(GithubCallbackError::MissingState)?;
    let cookie = cookie_value(headers, OAUTH_STATE_COOKIE)
        .filter(|cookie| !cookie.is_empty())
        .ok_or(GithubCallbackError::MissingCookie)?;
    if !ct_eq(state, cookie) {
        return Err(GithubCallbackError::StateMismatch);
    }
    Ok(())
}

#[axum::debug_handler]
async fn github_auth(Extension(state): Extension<AppState>) -> Result<Response, AppError> {
    // Random state parameter, validated in the callback, to prevent login
    // CSRF (an attacker tricking a victim's browser into completing the
    // attacker's own OAuth flow).
    let oauth_state = hex::encode(rand::random::<[u8; 16]>());

    let redirect_url = format!(
        "https://github.com/login/oauth/authorize?client_id={}&scope=user:email&redirect_uri={}/auth/github/callback&state={}",
        state.auth.github_client_id,
        state.host,
        oauth_state
    );
    let mut response = Redirect::temporary(&redirect_url).into_response();
    response.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        HeaderValue::from_str(&oauth_state_cookie(
            &oauth_state,
            state.host.starts_with("https://"),
            CHALLENGE_TTL.as_secs(),
        ))?,
    );
    Ok(response)
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
    // Verify the token was issued to *this* OAuth app. Without this check,
    // any GitHub token with email scope (e.g. harvested from an unrelated
    // app) could be exchanged for a faucet JWT in the victim's name.
    let check = state
        .auth
        .client
        .post(format!(
            "https://api.github.com/applications/{}/token",
            state.auth.github_client_id
        ))
        .basic_auth(
            &state.auth.github_client_id,
            Some(&state.auth.github_client_secret),
        )
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "rust-github-oauth")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .json(&json!({ "access_token": params.code }))
        .send()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if !check.status().is_success() {
        return Err(StatusCode::UNAUTHORIZED);
    }

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
    if state.users_cache.is_banned(&primary_email.email).await {
        warn!("User {} is banned!", primary_email.email);
        return Err(StatusCode::BAD_REQUEST);
    }

    info!("Authing user through device flow");

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
    Ok(Json(DeviceReturn { token }))
}

#[axum::debug_handler]
async fn github_callback(
    Query(params): Query<GithubCallback>,
    Extension(state): Extension<AppState>,
    headers: HeaderMap,
) -> Result<Response, GithubCallbackError> {
    // Validate the OAuth state parameter against the 10-minute HttpOnly cookie.
    validate_oauth_state(params.state.as_deref(), &headers).map_err(|error| {
        // Log the reason only: codes, state values, and cookies are credentials.
        warn!("GitHub OAuth callback rejected: {error:?}");
        error
    })?;

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
        .map_err(|_| GithubCallbackError::Internal)?
        .json::<auth::GithubTokenResponse>()
        .await
        .map_err(|_| GithubCallbackError::Internal)?;

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
        .map_err(|_| GithubCallbackError::Internal)?
        .json::<Vec<GithubEmail>>()
        .await
        .map_err(|_| GithubCallbackError::Internal)?;

    // Find primary email
    let primary_email: GithubEmail = user_emails
        .into_iter()
        .find(|email| email.primary && email.verified)
        .ok_or(GithubCallbackError::Internal)?;

    // Check if user is banned
    if state.users_cache.is_banned(&primary_email.email).await {
        warn!("User {} is banned!", primary_email.email);
        return Err(GithubCallbackError::Banned);
    }

    info!("Authing user through GitHub web flow");

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
    .map_err(|_| GithubCallbackError::Internal)?;

    // Redirect to frontend with token
    let mut response =
        Redirect::temporary(&format!("{}/?token={token}", state.host)).into_response();
    response.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        HeaderValue::from_str(&oauth_state_cookie(
            "",
            state.host.starts_with("https://"),
            0,
        ))
        .map_err(|_| GithubCallbackError::Internal)?,
    );
    Ok(response)
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
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<OnchainRequest>,
) -> Result<Json<OnchainResponse>, AppError> {
    let x_forwarded_for = client_ip(&headers, peer, state.trusted_gateway);

    let res = pay_onchain(&state, &x_forwarded_for, user, payload).await?;

    Ok(Json(res))
}

#[axum::debug_handler]
async fn lightning_handler(
    Extension(state): Extension<AppState>,
    Extension(user): Extension<AuthUser>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<LightningRequest>,
) -> Result<Json<LightningResponse>, AppError> {
    let x_forwarded_for = client_ip(&headers, peer, state.trusted_gateway);

    let payment_hash =
        pay_lightning(&state, &x_forwarded_for, Some(&user), &payload.bolt11).await?;

    Ok(Json(LightningResponse { payment_hash }))
}

#[axum::debug_handler]
async fn lnurlw_handler(
    Extension(state): Extension<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<WithdrawalResponse>, AppError> {
    let key = format!(
        "lnurlw:{}",
        client_ip(&headers, peer, state.trusted_gateway)
    );
    if !state
        .payments
        .try_reserve(&[(&key, INVOICE_REQ_DAILY_LIMIT)], 1)
        .await
    {
        return Err(AppError::new("Too many requests"));
    }

    // Random, single-use k1. The old k1 was a deterministic HMAC of the
    // client identity: it never expired and could be replayed forever.
    let k1 = hex::encode(rand::random::<[u8; 32]>());
    let now = chrono::Utc::now().timestamp();
    let cutoff = now - CHALLENGE_TTL.as_secs() as i64;
    sqlx::query("DELETE FROM lnurlw_challenges WHERE created_at <= ?")
        .bind(cutoff)
        .execute(&state.users_db)
        .await?;
    let inserted = sqlx::query(
        "INSERT INTO lnurlw_challenges (k1, created_at)
         SELECT ?, ? WHERE (SELECT COUNT(*) FROM lnurlw_challenges) < ?",
    )
    .bind(&k1)
    .bind(now)
    .bind(MAX_OUTSTANDING_LNURLW_CHALLENGES)
    .execute(&state.users_db)
    .await?;
    if inserted.rows_affected() != 1 {
        return Err(AppError::new("Too many outstanding withdrawal requests"));
    }

    let resp = WithdrawalResponse {
        default_description: "Mutinynet Faucet".to_string(),
        callback: "https://faucet.mutinynet.com/api/lnurlw/callback".to_string(),
        k1,
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
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(payload): Query<LnurlWithdrawParams>,
) -> Result<Json<Value>, Json<Value>> {
    let x_forwarded_for = client_ip(&headers, peer, state.trusted_gateway);

    // Consume the k1: it must exist, be unexpired, and is single-use.
    let cutoff = chrono::Utc::now().timestamp() - CHALLENGE_TTL.as_secs() as i64;
    let k1_valid = sqlx::query("DELETE FROM lnurlw_challenges WHERE k1 = ? AND created_at > ?")
        .bind(&payload.k1)
        .bind(cutoff)
        .execute(&state.users_db)
        .await
        .is_ok_and(|result| result.rows_affected() == 1);
    if !k1_valid {
        return Err(Json(json!({"status": "ERROR", "reason": "Incorrect k1"})));
    }

    // Only accept bolt11 invoices. pay_lightning also resolves LNURLs and
    // lightning addresses, which would let an unauthenticated caller make
    // the server fetch invoices from (and send requests to) arbitrary servers.
    if Bolt11Invoice::from_str(&payload.pr).is_err() {
        return Err(Json(
            json!({"status": "ERROR", "reason": "pr must be a bolt11 invoice"}),
        ));
    }

    // The rate limit is enforced atomically inside pay_lightning.
    pay_lightning(&state, &x_forwarded_for, None, &payload.pr)
        .await
        .map_err(|e| Json(json!({"status": "ERROR", "reason": format!("{e}")})))?;
    Ok(Json(json!({"status": "OK"})))
}

#[derive(Serialize)]
struct L402HandlerResponse {
    invoice: String,
    token: String,
    /// Lets the requester fetch the preimage from `/api/l402/check`. Only
    /// returned to the caller of `POST /api/l402`, never in the public 402
    /// challenge.
    claim: String,
}

/// Header carrying the claim secret to `/api/l402/check`. A header rather
/// than a query parameter so it stays out of URLs and access logs.
const L402_CLAIM_HEADER: &str = "x-l402-claim";

async fn generate_l402_challenge(state: &AppState) -> Result<L402HandlerResponse, AppError> {
    if !state.l402_config.enabled {
        return Err(AppError::new("L402 authentication is not enabled"));
    }

    let mainnet_client = state
        .mainnet_ldk_client
        .as_ref()
        .ok_or_else(|| AppError::new("Mainnet node not configured"))?;

    let response = generate_l402_token(
        mainnet_client,
        &state.auth.jwt_secret,
        state.l402_config.invoice_amount_sats,
    )
    .await?;

    if let Some(pool) = &state.analytics_db {
        analytics::record_l402_issued(
            pool,
            &response.payment_hash,
            state.l402_config.invoice_amount_sats,
        );
    }

    Ok(L402HandlerResponse {
        claim: l402::claim_secret(&state.auth.jwt_secret, &response.payment_hash),
        invoice: response.invoice,
        token: response.token,
    })
}

/// GET /api/l402 — returns 402 Payment Required with WWW-Authenticate header
/// for spec-compliant L402 discovery (e.g. 402index.io)
#[axum::debug_handler]
async fn l402_challenge_handler(
    Extension(state): Extension<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    // Unauthenticated invoice creation is rate-limited per IP to protect
    // the mainnet node from invoice spam.
    let key = format!("l402:{}", client_ip(&headers, peer, state.trusted_gateway));
    if !state
        .payments
        .try_reserve(&[(&key, INVOICE_REQ_DAILY_LIMIT)], 1)
        .await
    {
        return Err(AppError::new("Too many requests"));
    }

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
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<L402HandlerResponse>, AppError> {
    let key = format!("l402:{}", client_ip(&headers, peer, state.trusted_gateway));
    if !state
        .payments
        .try_reserve(&[(&key, INVOICE_REQ_DAILY_LIMIT)], 1)
        .await
    {
        return Err(AppError::new("Too many requests"));
    }

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
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(params): Query<L402CheckParams>,
) -> Result<Json<Value>, AppError> {
    if !state.l402_config.enabled {
        return Err(AppError::new("L402 authentication is not enabled"));
    }

    // Each check hits the mainnet node; rate-limit per IP.
    let key = format!(
        "l402check:{}",
        client_ip(&headers, peer, state.trusted_gateway)
    );
    if !state
        .payments
        .try_reserve(&[(&key, L402_CHECK_DAILY_LIMIT)], 1)
        .await
    {
        return Err(AppError::new("Too many requests"));
    }

    let mainnet_client = state
        .mainnet_ldk_client
        .as_ref()
        .ok_or_else(|| AppError::new("Mainnet node not configured"))?;

    // Decode the JWT to get the payment_hash
    let token_data = jsonwebtoken::decode::<l402::L402Claims>(
        &params.token,
        &jsonwebtoken::DecodingKey::from_secret(state.auth.jwt_secret.as_bytes()),
        &jsonwebtoken::Validation::default(),
    )
    .map_err(|_| AppError::new("Invalid token"))?;

    let payment_hash_hex = &token_data.claims.payment_hash;
    if hex::decode(payment_hash_hex).map_or(true, |b| b.len() != 32) {
        return Err(AppError::new("Invalid payment hash"));
    }

    // ldk-server keys inbound BOLT11 payments by their payment hash.
    let payment = mainnet_client
        .get_payment_details(GetPaymentDetailsRequest {
            payment_id: payment_hash_hex.clone(),
        })
        .await
        .map_err(|_| AppError::new("Failed to lookup invoice"))?
        .payment;

    let status = payment.as_ref().map(|p| p.status());
    if status == Some(PaymentStatus::Succeeded) {
        // The token is public by design (it travels in URLs and the 402
        // challenge), so the preimage is only returned alongside proof that
        // the caller requested this invoice. Anyone else learns the preimage
        // from their own Lightning payment.
        let claimed = headers
            .get(L402_CLAIM_HEADER)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|claim| {
                l402::verify_claim_secret(&state.auth.jwt_secret, payment_hash_hex, claim)
            });

        if claimed {
            let preimage = payment
                .and_then(|p| p.kind?.kind)
                .and_then(|kind| match kind {
                    payment_kind::Kind::Bolt11(bolt11) => bolt11.preimage,
                    _ => None,
                })
                .ok_or_else(|| AppError::new("Preimage unavailable"))?;
            Ok(Json(json!({
                "status": "settled",
                "preimage": preimage,
            })))
        } else {
            Ok(Json(json!({
                "status": "settled",
            })))
        }
    } else if status == Some(PaymentStatus::Failed)
        || chrono::Utc::now().timestamp() as usize
            > token_data.claims.iat + l402::L402_INVOICE_EXPIRY_SECS as usize
    {
        // ldk-server has no record of an invoice until it is paid, so
        // expiry is derived from when the token was issued.
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
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<Bolt11Request>,
) -> Result<Json<Bolt11Response>, AppError> {
    // Unauthenticated invoice creation is rate-limited per IP to protect
    // the LND node from invoice spam.
    let key = format!(
        "bolt11:{}",
        client_ip(&headers, peer, state.trusted_gateway)
    );
    if !state
        .payments
        .try_reserve(&[(&key, INVOICE_REQ_DAILY_LIMIT)], 1)
        .await
    {
        return Err(AppError::new("Too many requests"));
    }

    let bolt11 = request_bolt11(&state, payload.clone()).await?;

    Ok(Json(Bolt11Response { bolt11 }))
}

#[axum::debug_handler]
async fn channel_handler(
    Extension(state): Extension<AppState>,
    Extension(user): Extension<AuthUser>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<ChannelRequest>,
) -> Result<Json<ChannelResponse>, AppError> {
    let x_forwarded_for = client_ip(&headers, peer, state.trusted_gateway);

    let txid = open_channel(&state, &x_forwarded_for, Some(&user), payload).await?;

    Ok(Json(ChannelResponse { txid }))
}

#[derive(Serialize)]
struct LimitsResponse {
    /// Per-identifier daily cap in sats.
    max_daily_sats: u64,
    /// Sats sent in the last 24h attributable to this user (across IPs).
    user_used_sats: u64,
    /// Sats sent in the last 24h from the requesting IP (across users).
    ip_used_sats: u64,
    /// Sats this user can still send before hitting the most-restrictive cap.
    /// Always 0 until 24h have passed once a cap is reached. Premium users have no cap.
    remaining_sats: u64,
    /// True if the caller bypasses rate limits.
    is_premium: bool,
    /// Seconds in the rolling rate-limit window.
    window_seconds: u64,
}

#[axum::debug_handler]
async fn limits_handler(
    Extension(state): Extension<AppState>,
    Extension(user): Extension<AuthUser>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<LimitsResponse>, AppError> {
    let x_forwarded_for = client_ip(&headers, peer, state.trusted_gateway);

    let (ip_used, user_used) = state
        .payments
        .get_usage(&x_forwarded_for, Some(&user))
        .await;

    let remaining = if user.is_premium {
        MAX_SEND_AMOUNT
    } else {
        // The most-restrictive identifier wins (matches try_reserve_payment).
        MAX_SEND_AMOUNT.saturating_sub(ip_used.max(user_used))
    };

    Ok(Json(LimitsResponse {
        max_daily_sats: MAX_SEND_AMOUNT,
        user_used_sats: user_used,
        ip_used_sats: ip_used,
        remaining_sats: remaining,
        is_premium: user.is_premium,
        window_seconds: 86_400,
    }))
}

#[axum::debug_handler]
async fn arkade_handler(
    Extension(state): Extension<AppState>,
    Extension(user): Extension<AuthUser>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<ArkadeRequest>,
) -> Result<Json<ArkadeResponse>, ArkadeError> {
    let x_forwarded_for = client_ip(&headers, peer, state.trusted_gateway);

    let res = dispense_arkade(&state, &x_forwarded_for, &user, payload).await?;
    Ok(Json(res))
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

/// Extract the client identity used for rate limiting.
///
/// The TCP peer address is the identity by default. `X-Forwarded-For` is
/// only honored when the peer is a trusted proxy (loopback or the
/// container's default gateway, where the local nginx proxy arrives from).
/// nginx sets `X-Forwarded-For: $proxy_add_x_forwarded_for` with the real
/// client IP as the rightmost entry. Any earlier entries are
/// client-supplied and must not be trusted, so only the rightmost entry is
/// used. Direct connections from any other peer ignore the header entirely.
fn client_ip(headers: &HeaderMap, peer: SocketAddr, gateway: Option<IpAddr>) -> String {
    if is_trusted_proxy(&peer.ip(), gateway) {
        if let Some(xff) = headers
            .get("x-forwarded-for")
            .and_then(|x| HeaderValue::to_str(x).ok())
            .and_then(|x| x.rsplit(',').next())
            .map(str::trim)
            .filter(|x| !x.is_empty())
        {
            return xff.to_string();
        }
    }
    peer.ip().to_string()
}

/// X-Forwarded-For is only trusted when it comes from the local nginx
/// proxy: a loopback peer, or the container's default gateway (docker-proxy
/// hairpins host traffic, so nginx arrives with the gateway IP as its
/// peer). Do not broaden this to whole private ranges.
fn is_trusted_proxy(peer: &IpAddr, gateway: Option<IpAddr>) -> bool {
    peer.is_loopback() || gateway.is_some_and(|gateway| gateway == *peer)
}

/// Read the container's default gateway from /proc/net/route.
///
/// Returns None when there is no default route or the file cannot be
/// parsed (e.g. local dev outside a container); callers then trust
/// loopback only. Never fails.
fn default_gateway() -> Option<IpAddr> {
    let contents = std::fs::read_to_string("/proc/net/route").ok()?;
    parse_default_gateway(&contents)
}

/// Parse the default gateway from /proc/net/route contents: the gateway
/// field on the line whose destination is 00000000, as a little-endian hex
/// IPv4 address.
fn parse_default_gateway(contents: &str) -> Option<IpAddr> {
    for line in contents.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 || fields[1] != "00000000" {
            continue;
        }
        let Ok(raw) = u32::from_str_radix(fields[2], 16) else {
            continue;
        };
        if raw == 0 {
            continue;
        }
        return Some(IpAddr::V4(Ipv4Addr::from(raw.to_le_bytes())));
    }
    None
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
        // Log the full error chain server-side; the client only gets the
        // top-level message.
        error!("request failed: {:?}", self.0);
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

/// Constant-time string comparison for bearer tokens. The length still
/// leaks; tokens should be fixed-length random strings.
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn verify_bearer_token(headers: &HeaderMap, expected: &Option<String>) -> Result<(), StatusCode> {
    let token = expected.as_deref().ok_or(StatusCode::NOT_FOUND)?;
    let provided = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    if !ct_eq(provided, token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(())
}

async fn admin_auth_middleware<B>(
    headers: HeaderMap,
    request: Request<B>,
    next: Next<B>,
) -> Result<Response, StatusCode> {
    let state = request
        .extensions()
        .get::<AppState>()
        .expect("AppState not found in extensions");
    verify_bearer_token(&headers, &state.admin_token)?;
    Ok(next.run(request).await)
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
    verify_bearer_token(&headers, &state.analytics_token)?;
    Ok(next.run(request).await)
}

async fn fallback() -> (StatusCode, &'static str) {
    (StatusCode::NOT_FOUND, "Not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_state_cookie_is_bound_to_request_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_static("other=x; mutinynet_oauth_state=expected"),
        );

        assert_eq!(cookie_value(&headers, OAUTH_STATE_COOKIE), Some("expected"));
        assert!(ct_eq(
            cookie_value(&headers, OAUTH_STATE_COOKIE).unwrap(),
            "expected"
        ));
        assert!(!ct_eq(
            cookie_value(&headers, OAUTH_STATE_COOKIE).unwrap(),
            "attacker"
        ));
    }

    #[test]
    fn oauth_state_cookie_can_be_in_a_later_header() {
        let mut headers = HeaderMap::new();
        headers.append(
            axum::http::header::COOKIE,
            HeaderValue::from_static("other=x"),
        );
        headers.append(
            axum::http::header::COOKIE,
            HeaderValue::from_static("mutinynet_oauth_state=expected"),
        );

        assert_eq!(cookie_value(&headers, OAUTH_STATE_COOKIE), Some("expected"));
    }

    #[test]
    fn oauth_state_validation_rejects_missing_empty_and_mismatched_values() {
        let mut headers = HeaderMap::new();
        assert_eq!(
            validate_oauth_state(None, &headers),
            Err(GithubCallbackError::MissingState)
        );
        assert_eq!(
            validate_oauth_state(Some("expected"), &headers),
            Err(GithubCallbackError::MissingCookie)
        );
        headers.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_static("mutinynet_oauth_state="),
        );
        assert_eq!(
            validate_oauth_state(Some(""), &headers),
            Err(GithubCallbackError::MissingState)
        );
        assert_eq!(
            validate_oauth_state(Some("expected"), &headers),
            Err(GithubCallbackError::MissingCookie)
        );
        headers.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_static("mutinynet_oauth_state=expected"),
        );
        assert_eq!(
            validate_oauth_state(Some("attacker"), &headers),
            Err(GithubCallbackError::StateMismatch)
        );
        assert_eq!(validate_oauth_state(Some("expected"), &headers), Ok(()));
    }

    #[tokio::test]
    async fn oauth_callback_errors_explain_the_failure() {
        use axum::body::HttpBody;

        for (error, status, expected_message) in [
            (
                GithubCallbackError::MissingState,
                StatusCode::BAD_REQUEST,
                "state is missing",
            ),
            (
                GithubCallbackError::MissingCookie,
                StatusCode::BAD_REQUEST,
                "cookie is missing or expired",
            ),
            (
                GithubCallbackError::StateMismatch,
                StatusCode::BAD_REQUEST,
                "state does not match",
            ),
            (
                GithubCallbackError::Banned,
                StatusCode::FORBIDDEN,
                "not allowed",
            ),
            (
                GithubCallbackError::Internal,
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not be completed",
            ),
        ] {
            let response = error.into_response();
            assert_eq!(response.status(), status);
            let mut body = response.into_body();
            let bytes = body.data().await.unwrap().unwrap();
            assert!(std::str::from_utf8(&bytes)
                .unwrap()
                .contains(expected_message));
        }
    }

    /// A request that arrives through the local nginx proxy (loopback peer)
    /// uses the rightmost X-Forwarded-For entry as its identity.
    #[test]
    fn xff_from_loopback_peer_is_used_as_identity() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("1.2.3.4, 203.0.113.66"),
        );
        let peer = SocketAddr::from(([127, 0, 0, 1], 12345));
        assert_eq!(client_ip(&headers, peer, None), "203.0.113.66");
    }

    /// docker-proxy hairpins nginx traffic, so nginx arrives with the
    /// container's default gateway as its peer. XFF is trusted from there.
    #[test]
    fn xff_from_gateway_peer_is_used_as_identity() {
        let gateway = Some(IpAddr::V4(Ipv4Addr::new(172, 18, 0, 1)));
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.66"));
        let peer = SocketAddr::from(([172, 18, 0, 1], 12345));
        assert_eq!(client_ip(&headers, peer, gateway), "203.0.113.66");
    }

    /// A spoofed X-Forwarded-For header from a direct (untrusted) peer must
    /// not become the rate-limit identity: the TCP peer address wins.
    #[test]
    fn spoofed_xff_from_direct_peer_is_ignored() {
        let gateway = Some(IpAddr::V4(Ipv4Addr::new(172, 18, 0, 1)));
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.66"));
        // Another container on the same bridge is not the gateway.
        let peer = SocketAddr::from(([172, 18, 0, 5], 12345));
        assert_eq!(client_ip(&headers, peer, gateway), "172.18.0.5");
        // A public peer is not trusted either.
        let peer = SocketAddr::from(([198, 51, 100, 7], 12345));
        assert_eq!(client_ip(&headers, peer, gateway), "198.51.100.7");
    }

    /// Without XFF, a loopback peer falls back to its own address.
    #[test]
    fn loopback_peer_without_xff_uses_peer_address() {
        let headers = HeaderMap::new();
        let peer = SocketAddr::from(([127, 0, 0, 1], 12345));
        assert_eq!(client_ip(&headers, peer, None), "127.0.0.1");
    }

    #[test]
    fn trusted_proxy_covers_loopback_and_exact_gateway_only() {
        let gateway = Some(IpAddr::V4(Ipv4Addr::new(172, 18, 0, 1)));
        assert!(is_trusted_proxy(&IpAddr::V4(Ipv4Addr::LOCALHOST), gateway));
        assert!(is_trusted_proxy(
            &IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
            gateway
        ));
        assert!(is_trusted_proxy(
            &IpAddr::V4(Ipv4Addr::new(172, 18, 0, 1)),
            gateway
        ));
        // Other private IPs on the same bridge are not trusted.
        assert!(!is_trusted_proxy(
            &IpAddr::V4(Ipv4Addr::new(172, 18, 0, 5)),
            gateway
        ));
        assert!(!is_trusted_proxy(
            &IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)),
            gateway
        ));
        // Without a detected gateway, only loopback is trusted.
        assert!(is_trusted_proxy(&IpAddr::V4(Ipv4Addr::LOCALHOST), None));
        assert!(!is_trusted_proxy(
            &IpAddr::V4(Ipv4Addr::new(172, 18, 0, 1)),
            None
        ));
    }

    #[test]
    fn parses_default_gateway_from_route_table() {
        let fixture =
            "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n\
                       eth0\t00000000\t0102A8C0\t0003\t0\t0\t0\t00000000\t0\t0\t0\n\
                       eth0\t0002A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(
            parse_default_gateway(fixture),
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 2, 1)))
        );
    }

    #[test]
    fn no_default_route_yields_no_gateway() {
        let fixture =
            "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n\
                       eth0\t0002A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(parse_default_gateway(fixture), None);
    }

    /// Rotating spoofed XFF values from a direct peer all collapse to the
    /// same identity, so the per-identity payout budget still applies.
    #[tokio::test]
    async fn rotating_spoofed_ips_no_longer_grant_unbounded_withdrawals() {
        let payments = PaymentsByIp::new();
        let peer = SocketAddr::from(([198, 51, 100, 7], 12345));

        let identities: Vec<String> = (0..10u32)
            .map(|i| {
                let mut headers = HeaderMap::new();
                headers.insert(
                    "x-forwarded-for",
                    HeaderValue::from_str(&format!("203.0.113.{i}")).unwrap(),
                );
                client_ip(&headers, peer, None)
            })
            .collect();
        assert!(identities.iter().all(|id| id == "198.51.100.7"));

        assert!(
            payments
                .try_reserve(&[("198.51.100.7", MAX_SEND_AMOUNT)], MAX_SEND_AMOUNT)
                .await
        );
        assert!(
            !payments
                .try_reserve(&[("198.51.100.7", MAX_SEND_AMOUNT)], 1)
                .await
        );
    }
}

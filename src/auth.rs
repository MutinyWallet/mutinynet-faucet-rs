use crate::l402::{validate_l402_credentials, L402Error};
use crate::AppState;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use jsonwebtoken::{decode, DecodingKey, Validation};
use log::info;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

#[derive(Clone)]
pub struct AuthState {
    pub client: Client,
    pub github_client_id: String,
    pub github_client_secret: String,
    pub jwt_secret: String,
}

#[derive(Deserialize)]
pub struct GithubCallback {
    pub code: String,
}

#[derive(Serialize, Deserialize)]
pub struct TokenClaims {
    pub sub: String,
    pub exp: usize,
    pub iat: usize,
}

#[derive(Deserialize)]
pub struct GithubTokenResponse {
    pub access_token: String,
}

// Custom error type for auth failures
#[derive(Debug, Clone, Copy)]
pub enum AuthError {
    InvalidToken,
    MissingToken,
    TokenExpired,
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AuthError::InvalidToken => (StatusCode::UNAUTHORIZED, "Invalid token"),
            AuthError::MissingToken => (StatusCode::UNAUTHORIZED, "Missing token"),
            AuthError::TokenExpired => (StatusCode::UNAUTHORIZED, "Token expired"),
        };

        (
            status,
            Json(serde_json::json!({
                "error": message
            })),
        )
            .into_response()
    }
}

pub async fn init_users_db(path: &str) -> anyhow::Result<SqlitePool> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    let pool = SqlitePool::connect(&format!("sqlite:{}?mode=rwc", path)).await?;

    sqlx::query("PRAGMA journal_mode=WAL")
        .execute(&pool)
        .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS banned_domains (
            domain TEXT PRIMARY KEY NOT NULL
        )",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS banned_users (
            email TEXT PRIMARY KEY NOT NULL
        )",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS whitelisted_users (
            email TEXT PRIMARY KEY NOT NULL
        )",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS premium_users (
            email TEXT PRIMARY KEY NOT NULL
        )",
    )
    .execute(&pool)
    .await?;

    // Migrate from text files if tables are empty and files exist
    migrate_from_files(&pool).await;

    Ok(pool)
}

async fn migrate_from_files(pool: &SqlitePool) {
    let files: &[(&str, &str, &str)] = &[
        (
            "faucet_config/banned_domains.txt",
            "banned_domains",
            "INSERT OR IGNORE INTO banned_domains (domain) VALUES (?)",
        ),
        (
            "faucet_config/banned_users.txt",
            "banned_users",
            "INSERT OR IGNORE INTO banned_users (email) VALUES (?)",
        ),
        (
            "faucet_config/whitelisted_users.txt",
            "whitelisted_users",
            "INSERT OR IGNORE INTO whitelisted_users (email) VALUES (?)",
        ),
        (
            "faucet_config/premium_users.txt",
            "premium_users",
            "INSERT OR IGNORE INTO premium_users (email) VALUES (?)",
        ),
    ];

    for (file_path, table_name, insert_sql) in files {
        // Skip if the text file doesn't exist
        let contents = match std::fs::read_to_string(file_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Skip if the table already has data (already migrated)
        let query = format!("SELECT COUNT(*) FROM {}", table_name);
        let count: (i64,) = sqlx::query_as(&query)
            .fetch_one(pool)
            .await
            .unwrap_or((1,));
        if count.0 > 0 {
            info!(
                "Skipping migration of {} — table {} already has {} entries",
                file_path, table_name, count.0
            );
            continue;
        }

        let mut migrated = 0u32;
        let mut tx = match pool.begin().await {
            Ok(tx) => tx,
            Err(_) => continue,
        };
        for line in contents.lines() {
            let line = line.trim();
            if !line.is_empty() {
                let _ = sqlx::query(insert_sql).bind(line).execute(&mut *tx).await;
                migrated += 1;
            }
        }
        let _ = tx.commit().await;
        info!("Migrated {} entries from {} into {}", migrated, file_path, table_name);
    }
}

pub struct UserStatus {
    pub is_banned: bool,
    pub is_premium: bool,
}

/// Single-query check for ban and premium status.
pub async fn check_user_status(pool: &SqlitePool, email: &str) -> UserStatus {
    let domain = email.split('@').next_back().unwrap_or("");
    let result: (i32, i32, i32, i32) = sqlx::query_as(
        "SELECT
            EXISTS(SELECT 1 FROM whitelisted_users WHERE email = ?1),
            EXISTS(SELECT 1 FROM premium_users WHERE email = ?1),
            EXISTS(SELECT 1 FROM banned_domains WHERE domain = LOWER(?2)),
            EXISTS(SELECT 1 FROM banned_users WHERE email = ?1)",
    )
    .bind(email)
    .bind(domain)
    .fetch_one(pool)
    .await
    .unwrap_or((0, 0, 0, 0));

    let whitelisted = result.0 != 0;
    let premium = result.1 != 0;
    let domain_banned = result.2 != 0;
    let user_banned = result.3 != 0;

    UserStatus {
        is_premium: premium,
        is_banned: !whitelisted && !premium && (domain_banned || user_banned),
    }
}

pub async fn is_banned(pool: &SqlitePool, email: &str) -> bool {
    check_user_status(pool, email).await.is_banned
}

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub username: String,
    pub is_premium: bool,
}

// Middleware for JWT and L402 verification
pub async fn auth_middleware<B>(
    headers: HeaderMap,
    mut request: Request<B>,
    next: Next<B>,
) -> Result<Response, AuthError> {
    let state = request
        .extensions()
        .get::<AppState>()
        .expect("AppState not found in extensions");

    let auth_header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(AuthError::MissingToken)?;

    let auth_user = if let Some(token) = auth_header.strip_prefix("Bearer ") {
        // GitHub OAuth JWT path
        if token.is_empty() {
            return Err(AuthError::MissingToken);
        }

        let token_data = decode::<TokenClaims>(
            token,
            &DecodingKey::from_secret(state.auth.jwt_secret.as_bytes()),
            &Validation::default(),
        )
        .map_err(|_| AuthError::InvalidToken)?;

        let now = chrono::Utc::now().timestamp() as usize;
        if token_data.claims.exp < now {
            return Err(AuthError::TokenExpired);
        }

        let status = check_user_status(&state.users_db, &token_data.claims.sub).await;
        if status.is_banned {
            return Err(AuthError::TokenExpired);
        }

        AuthUser {
            username: token_data.claims.sub,
            is_premium: status.is_premium,
        }
    } else if let Some(credentials) = auth_header.strip_prefix("L402 ") {
        // L402 Lightning payment path
        let (token, preimage_hex) = credentials.split_once(':').ok_or(AuthError::InvalidToken)?;

        if token.is_empty() || preimage_hex.is_empty() {
            return Err(AuthError::MissingToken);
        }

        let payment_hash = validate_l402_credentials(token, preimage_hex, &state.auth.jwt_secret)
            .map_err(|e| match e {
            L402Error::TokenExpired => AuthError::TokenExpired,
            _ => AuthError::InvalidToken,
        })?;

        if let Some(pool) = &state.analytics_db {
            crate::analytics::record_l402_paid(pool, &payment_hash);
        }

        AuthUser {
            username: format!("l402:{}", payment_hash),
            is_premium: false,
        }
    } else {
        return Err(AuthError::InvalidToken);
    };

    request.extensions_mut().insert(auth_user);
    Ok(next.run(request).await)
}

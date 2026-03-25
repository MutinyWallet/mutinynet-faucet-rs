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
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;

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

struct UserSets {
    banned_domains: HashSet<String>,
    banned_users: HashSet<String>,
    whitelisted_users: HashSet<String>,
    premium_users: HashSet<String>,
}

pub struct UsersCache {
    sets: RwLock<UserSets>,
}

impl UsersCache {
    pub async fn load(pool: &SqlitePool) -> anyhow::Result<Arc<Self>> {
        let banned_domains = load_set(pool, "SELECT domain FROM banned_domains").await?;
        let banned_users = load_set(pool, "SELECT email FROM banned_users").await?;
        let whitelisted_users = load_set(pool, "SELECT email FROM whitelisted_users").await?;
        let premium_users = load_set(pool, "SELECT email FROM premium_users").await?;

        info!(
            "Users cache loaded: {} banned domains, {} banned users, {} whitelisted, {} premium",
            banned_domains.len(),
            banned_users.len(),
            whitelisted_users.len(),
            premium_users.len(),
        );

        Ok(Arc::new(Self {
            sets: RwLock::new(UserSets {
                banned_domains,
                banned_users,
                whitelisted_users,
                premium_users,
            }),
        }))
    }

    pub async fn check_status(&self, email: &str) -> UserStatus {
        let sets = self.sets.read().await;
        let whitelisted = sets.whitelisted_users.contains(email);
        let premium = sets.premium_users.contains(email);
        let domain = email.split('@').next_back().unwrap_or("");
        let domain_banned = sets.banned_domains.contains(&domain.to_lowercase());
        let user_banned = sets.banned_users.contains(email);

        UserStatus {
            is_premium: premium,
            is_banned: !whitelisted && !premium && (domain_banned || user_banned),
        }
    }

    pub async fn is_banned(&self, email: &str) -> bool {
        self.check_status(email).await.is_banned
    }

    pub async fn list(&self, list: &str) -> Option<Vec<String>> {
        let sets = self.sets.read().await;
        let set = match list {
            "banned_domains" => &sets.banned_domains,
            "banned_users" => &sets.banned_users,
            "whitelisted_users" => &sets.whitelisted_users,
            "premium_users" => &sets.premium_users,
            _ => return None,
        };
        let mut entries: Vec<String> = set.iter().cloned().collect();
        entries.sort();
        Some(entries)
    }

    pub async fn add(&self, list: &str, value: String) {
        let mut sets = self.sets.write().await;
        let set = match list {
            "banned_domains" => &mut sets.banned_domains,
            "banned_users" => &mut sets.banned_users,
            "whitelisted_users" => &mut sets.whitelisted_users,
            "premium_users" => &mut sets.premium_users,
            _ => return,
        };
        set.insert(value);
    }

    pub async fn remove(&self, list: &str, value: &str) {
        let mut sets = self.sets.write().await;
        let set = match list {
            "banned_domains" => &mut sets.banned_domains,
            "banned_users" => &mut sets.banned_users,
            "whitelisted_users" => &mut sets.whitelisted_users,
            "premium_users" => &mut sets.premium_users,
            _ => return,
        };
        set.remove(value);
    }
}

async fn load_set(pool: &SqlitePool, query: &str) -> anyhow::Result<HashSet<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(query).fetch_all(pool).await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
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

        let status = state.users_cache.check_status(&token_data.claims.sub).await;
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

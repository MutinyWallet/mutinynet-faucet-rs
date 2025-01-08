use crate::AppState;
use axum::headers::authorization::Bearer;
use axum::headers::Authorization;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Json, TypedHeader};
use jsonwebtoken::{decode, DecodingKey, Validation};
use reqwest::Client;
use serde::{Deserialize, Serialize};

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

#[derive(Deserialize)]
pub struct GithubUser {
    pub id: i64,
    pub login: String,
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

fn banned_domains() -> Vec<String> {
    let mut domains = vec![];
    let file = std::fs::read_to_string("faucet_config/banned_domains.txt");
    if let Ok(file) = file {
        for line in file.lines() {
            let line = line.trim();
            if !line.is_empty() {
                domains.push(line.to_string());
            }
        }
    }
    domains
}

fn get_banned_users() -> Vec<String> {
    let mut banned_users = vec![];
    let file = std::fs::read_to_string("faucet_config/banned_users.txt");
    if let Ok(file) = file {
        for line in file.lines() {
            let line = line.trim();
            if !line.is_empty() {
                banned_users.push(line.to_string());
            }
        }
    }
    banned_users
}

fn get_whitelisted_users() -> Vec<String> {
    let mut whitelisted_users = vec![];
    let file = std::fs::read_to_string("faucet_config/whitelisted_users.txt");
    if let Ok(file) = file {
        for line in file.lines() {
            let line = line.trim();
            if !line.is_empty() {
                whitelisted_users.push(line.to_string());
            }
        }
    }
    whitelisted_users
}

pub fn is_banned(email: &String) -> bool {
    let whitelisted_users = get_whitelisted_users();
    if whitelisted_users.contains(email) {
        return false;
    }
    let domains = banned_domains();
    let user_host = email.split('@').last().unwrap_or("");
    if domains.contains(&user_host.to_lowercase()) {
        return true;
    }
    let banned_users = get_banned_users();
    banned_users.contains(email)
}

// Middleware extractor for authenticated users
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub username: String,
}

// Middleware for JWT verification
pub async fn auth_middleware<B>(
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    mut request: Request<B>,
    next: Next<B>,
) -> Result<Response, AuthError> {
    let state = request
        .extensions()
        .get::<AppState>()
        .expect("JWT config not found in extensions");

    if auth.token().is_empty() {
        return Err(AuthError::MissingToken);
    }

    // Verify and decode the token
    let token_data = decode::<TokenClaims>(
        auth.token(),
        &DecodingKey::from_secret(state.auth.jwt_secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(|_| AuthError::InvalidToken)?;

    // Check if token is expired
    let now = chrono::Utc::now().timestamp() as usize;
    if token_data.claims.exp < now {
        return Err(AuthError::TokenExpired);
    }

    if is_banned(&token_data.claims.sub) {
        return Err(AuthError::TokenExpired);
    }

    // Add AuthUser to request extensions
    request.extensions_mut().insert(AuthUser {
        username: token_data.claims.sub,
    });

    Ok(next.run(request).await)
}

use crate::l402::{validate_l402_credentials, L402Error};
use crate::AppState;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
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

fn get_premium_users() -> Vec<String> {
    let mut premium_users = vec![];
    let file = std::fs::read_to_string("faucet_config/premium_users.txt");
    if let Ok(file) = file {
        for line in file.lines() {
            let line = line.trim();
            if !line.is_empty() {
                premium_users.push(line.to_string());
            }
        }
    }
    premium_users
}

pub fn is_premium(email: &String) -> bool {
    let premium_users = get_premium_users();
    premium_users.contains(email)
}

pub fn is_banned(email: &String) -> bool {
    let whitelisted_users = get_whitelisted_users();
    if whitelisted_users.contains(email) {
        return false;
    }
    if is_premium(email) {
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

        if is_banned(&token_data.claims.sub) {
            return Err(AuthError::TokenExpired);
        }

        AuthUser {
            username: token_data.claims.sub,
        }
    } else if let Some(credentials) = auth_header.strip_prefix("L402 ") {
        // L402 Lightning payment path
        let (token, preimage_hex) = credentials
            .split_once(':')
            .ok_or(AuthError::InvalidToken)?;

        if token.is_empty() || preimage_hex.is_empty() {
            return Err(AuthError::MissingToken);
        }

        let payment_hash =
            validate_l402_credentials(token, preimage_hex, &state.auth.jwt_secret).map_err(
                |e| match e {
                    L402Error::TokenExpired => AuthError::TokenExpired,
                    _ => AuthError::InvalidToken,
                },
            )?;

        AuthUser {
            username: format!("l402:{}", payment_hash),
        }
    } else {
        return Err(AuthError::InvalidToken);
    };

    request.extensions_mut().insert(auth_user);
    Ok(next.run(request).await)
}

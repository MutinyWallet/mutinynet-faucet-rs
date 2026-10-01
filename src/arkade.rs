use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use log::info;
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::{AppState, MAX_SEND_AMOUNT};

#[derive(Clone, Deserialize)]
pub struct ArkadeRequest {
    pub address: String,
    pub sats: u64,
}

#[derive(Clone, Serialize)]
pub struct ArkadeResponse {
    pub txid: String,
}

#[derive(Debug)]
pub struct ArkadeError {
    status: StatusCode,
    error: anyhow::Error,
}

impl From<anyhow::Error> for ArkadeError {
    fn from(error: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error,
        }
    }
}

impl IntoResponse for ArkadeError {
    fn into_response(self) -> Response {
        log::error!("arkade request failed: {:?}", self.error);
        (
            self.status,
            Json(serde_json::json!({ "error": self.error.to_string() })),
        )
            .into_response()
    }
}

pub async fn dispense_arkade(
    state: &AppState,
    x_forwarded_for: &str,
    user: &AuthUser,
    payload: ArkadeRequest,
) -> Result<ArkadeResponse, ArkadeError> {
    let daemon_url = state
        .arkade_daemon_url
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Arkade daemon not configured"))?;

    if payload.sats == 0 {
        return Err(anyhow::anyhow!("sats must be positive").into());
    }
    if payload.sats > MAX_SEND_AMOUNT {
        return Err(anyhow::anyhow!("max amount is {MAX_SEND_AMOUNT}").into());
    }

    // Atomically check the limits and record the payment before dispensing.
    // Premium users bypass the limit but are still tracked.
    if user.is_premium {
        state
            .payments
            .add_payment(x_forwarded_for, None, Some(user), payload.sats)
            .await;
    } else if !state
        .payments
        .try_reserve_payment(x_forwarded_for, None, Some(user), payload.sats)
        .await
    {
        return Err(anyhow::anyhow!("Too many payments").into());
    }

    // Keep connection details and server error bodies out of client responses.
    let daemon_url = daemon_url.trim_end_matches('/');
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(anyhow::Error::from)?;
    let mut req = client
        .post(format!("{daemon_url}/send"))
        .json(&serde_json::json!({ "address": payload.address, "sats": payload.sats }));

    if let Some(token) = state.arkade_internal_token.as_deref() {
        req = req.header("X-Internal-Token", token);
    }

    let json = send_request(req).await?;
    let txid = json
        .get("txid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("arkade daemon returned no txid"))?
        .to_string();

    info!(
        "arkade dispensed {} sats to {}",
        payload.sats, payload.address
    );

    if let Some(tx) = &state.analytics_writer {
        crate::analytics::record_payment(
            tx,
            "arkade",
            payload.sats,
            Some(&user.username),
            x_forwarded_for,
            Some(&payload.address),
        );
    }

    Ok(ArkadeResponse { txid })
}

async fn send_request(req: reqwest::RequestBuilder) -> Result<serde_json::Value, ArkadeError> {
    let resp = req.send().await.map_err(|e| {
        log::error!("arkade daemon request failed: {e}");
        anyhow::anyhow!("arkade dispenser unavailable")
    })?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.map_err(|e| {
            log::error!("arkade daemon response failed ({status}): {e}");
            anyhow::anyhow!("arkade dispenser unavailable")
        })?;
        log::error!("arkade daemon returned {status}: {body}");
        if status.is_client_error() {
            return Err(ArkadeError {
                status: StatusCode::from_u16(status.as_u16()).expect("valid HTTP status"),
                error: anyhow::anyhow!(body),
            });
        }
        return Err(anyhow::anyhow!("arkade dispenser unavailable").into());
    }

    resp.json().await.map_err(|e| {
        log::error!("arkade daemon response failed: {e}");
        if e.is_timeout() || e.is_body() {
            anyhow::anyhow!("arkade dispenser unavailable").into()
        } else {
            anyhow::Error::from(e).into()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::HttpBody, routing::post, Router};

    async fn assert_error_response(error: ArkadeError, status: StatusCode, message: &str) {
        let response = error.into_response();
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()["content-type"], "application/json");
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(chunk) = body.data().await {
            bytes.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            serde_json::json!({ "error": message })
        );
    }

    #[tokio::test]
    async fn daemon_errors_preserve_client_messages_and_hide_server_messages() {
        for (status, message) in [
            (
                StatusCode::BAD_REQUEST,
                "amount exceeds per-request cap of deployment-configured sats",
            ),
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid ark address"),
            (StatusCode::INTERNAL_SERVER_ERROR, "internal daemon failure"),
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let app = Router::new().route("/send", post(move || async move { (status, message) }));
            let server = tokio::spawn(
                axum::Server::from_tcp(listener)
                    .unwrap()
                    .serve(app.into_make_service()),
            );
            let result = send_request(
                reqwest::Client::new()
                    .post(format!("http://{address}/send"))
                    .json(&serde_json::json!({ "address": "test-address", "sats": 1 })),
            )
            .await;
            server.abort();

            assert_error_response(
                result.unwrap_err(),
                status,
                if status.is_client_error() {
                    message
                } else {
                    "arkade dispenser unavailable"
                },
            )
            .await;
        }
    }

    #[tokio::test]
    async fn connection_failure_returns_unavailable() {
        // Reserve a port without listening so connection attempts are refused.
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = socket.local_addr().unwrap();
        let error = send_request(reqwest::Client::new().post(format!("http://{address}/send")))
            .await
            .unwrap_err();
        assert_error_response(
            error,
            StatusCode::INTERNAL_SERVER_ERROR,
            "arkade dispenser unavailable",
        )
        .await;
    }

    #[tokio::test]
    async fn timeout_returns_unavailable() {
        // Accept TCP connections but never send an HTTP response.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let error = send_request(
            reqwest::Client::new()
                .post(format!("http://{address}/send"))
                .timeout(std::time::Duration::from_millis(50)),
        )
        .await
        .unwrap_err();
        assert_error_response(
            error,
            StatusCode::INTERNAL_SERVER_ERROR,
            "arkade dispenser unavailable",
        )
        .await;
    }
}

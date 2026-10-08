use std::str::FromStr;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lightning_invoice::Bolt11Invoice;
use lnurl::LnUrlResponse;
use log::info;
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::lightning::{
    get_lnurl_invoice, make_lnurl_request, parse_lnurl, validate_lnurl_invoice_amount,
};
use crate::payments::PaymentsByIp;
use crate::{AppState, MAX_SEND_AMOUNT};

#[derive(Clone, Deserialize)]
pub struct ArkadeRequest {
    #[serde(alias = "address")]
    pub destination: String,
    pub sats: u64,
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

fn client_error(status: StatusCode, message: String) -> ArkadeError {
    ArkadeError {
        status,
        error: anyhow::anyhow!(message),
    }
}

pub async fn dispense_arkade(
    state: &AppState,
    x_forwarded_for: &str,
    user: &AuthUser,
    payload: ArkadeRequest,
) -> Result<serde_json::Value, ArkadeError> {
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

    let requested = payload.destination.trim().trim_matches('"');
    let destination = resolve_lnurl(requested, payload.sats).await?;

    // Keep connection details and server error bodies out of client responses.
    let daemon_url = daemon_url.trim_end_matches('/');
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(anyhow::Error::from)?;
    let mut req = client
        .post(format!("{daemon_url}/send"))
        .json(&serde_json::json!({ "address": destination, "sats": payload.sats }));

    if let Some(token) = state.arkade_internal_token.as_deref() {
        req = req.header("X-Internal-Token", token);
    }

    let paid = send_reserved(&state.payments, req, x_forwarded_for, user, payload.sats).await?;
    let amount = paid["amount"].as_u64().unwrap_or(payload.sats);

    info!("arkade dispensed {amount} sats to {requested}");

    if let Some(tx) = &state.analytics_writer {
        crate::analytics::record_payment(
            tx,
            "arkade",
            amount,
            Some(&user.username),
            x_forwarded_for,
            Some(requested),
        );
    }

    Ok(paid)
}

/// Lightning addresses and LNURL-pay become a bolt11 for `sats`; other destinations pass through.
async fn resolve_lnurl(destination: &str, sats: u64) -> Result<String, ArkadeError> {
    let Some(lnurl) = parse_lnurl(destination) else {
        return Ok(destination.to_owned());
    };
    let LnUrlResponse::LnUrlPayResponse(pay) = make_lnurl_request(&lnurl.url).await? else {
        return Err(client_error(
            StatusCode::BAD_REQUEST,
            "That LNURL is not a pay request.".into(),
        ));
    };
    let msats = sats * 1_000;
    if msats < pay.min_sendable || msats > pay.max_sendable {
        return Err(client_error(
            StatusCode::BAD_REQUEST,
            format!(
                "This Lightning address accepts {}–{} sats.",
                pay.min_sendable.div_ceil(1_000),
                pay.max_sendable / 1_000
            ),
        ));
    }
    let invoice = Bolt11Invoice::from_str(get_lnurl_invoice(&pay, msats, None).await?.invoice())
        .map_err(|error| anyhow::anyhow!("invalid invoice: {error:?}"))?;
    validate_lnurl_invoice_amount(&invoice, msats)?;
    Ok(invoice.to_string())
}

async fn send_reserved(
    payments: &PaymentsByIp,
    req: reqwest::RequestBuilder,
    ip: &str,
    user: &AuthUser,
    sats: u64,
) -> Result<serde_json::Value, ArkadeError> {
    // Premium users bypass the limit but are still tracked.
    if user.is_premium {
        payments.add_payment(ip, None, Some(user), sats).await;
    } else if !payments
        .try_reserve_payment(ip, None, Some(user), sats)
        .await
    {
        let (ip_used, user_used) = payments.get_usage(ip, Some(user)).await;
        let remaining = MAX_SEND_AMOUNT.saturating_sub(ip_used.max(user_used));
        return Err(client_error(
            StatusCode::TOO_MANY_REQUESTS,
            format!("That's {sats} sats; you have {remaining} left in your 24h limit."),
        ));
    }

    let result = send_request(req).await;
    match &result {
        // A destination's own amount can undercut sats; count what was paid.
        Ok(paid) => {
            if let Some(amount) = paid["amount"].as_u64().filter(|amount| *amount < sats) {
                payments.release_payment(ip, None, Some(user), sats).await;
                payments.add_payment(ip, None, Some(user), amount).await;
            }
        }
        // The daemon answers 4xx only when nothing left its wallet.
        Err(error) if error.status.is_client_error() => {
            payments.release_payment(ip, None, Some(user), sats).await;
        }
        Err(_) => {}
    }
    result
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

    const IP: &str = "1.2.3.4";

    fn alice() -> AuthUser {
        AuthUser {
            username: "alice".into(),
            is_premium: false,
        }
    }

    /// A request to a daemon whose /send answers `status`, reporting `amount` paid.
    fn fake_daemon(status: StatusCode, amount: u64) -> reqwest::RequestBuilder {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/send",
            post(move || async move {
                let paid = serde_json::json!({ "rail": "ark", "status": "settled", "amount": amount, "txid": "t1" });
                (status, Json(paid))
            }),
        );
        tokio::spawn(
            axum::Server::from_tcp(listener)
                .unwrap()
                .serve(app.into_make_service()),
        );
        reqwest::Client::new().post(format!("http://{address}/send"))
    }

    async fn usage(payments: &PaymentsByIp) -> (u64, u64) {
        payments.get_usage(IP, Some(&alice())).await
    }

    #[tokio::test]
    async fn the_daemon_answer_settles_a_50k_reservation() {
        // (daemon status, sats it reports paid, sats left reserved)
        for (status, paid, reserved) in [
            (StatusCode::OK, 50_000, 50_000),
            (StatusCode::OK, 21_000, 21_000),
            (StatusCode::CONFLICT, 0, 0),
            (StatusCode::INTERNAL_SERVER_ERROR, 0, 50_000),
        ] {
            let payments = PaymentsByIp::new();
            match send_reserved(&payments, fake_daemon(status, paid), IP, &alice(), 50_000).await {
                Ok(json) => assert!(status.is_success() && json["txid"] == "t1"),
                Err(error) => assert_eq!(error.status, status),
            }
            assert_eq!(usage(&payments).await, (reserved, reserved), "{status}");
        }
    }

    #[tokio::test]
    async fn over_quota_says_what_is_left() {
        let payments = PaymentsByIp::new();
        payments
            .add_payment(IP, None, Some(&alice()), 960_000)
            .await;
        let err = send_reserved(
            &payments,
            fake_daemon(StatusCode::OK, 50_000),
            IP,
            &alice(),
            50_000,
        )
        .await
        .unwrap_err();
        assert_eq!(err.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            err.error.to_string(),
            "That's 50000 sats; you have 40000 left in your 24h limit."
        );
        assert_eq!(usage(&payments).await, (960_000, 960_000));
    }

    #[test]
    fn old_address_field_is_still_accepted() {
        let req: ArkadeRequest = serde_json::from_str(r#"{"address":"tark1x","sats":5}"#).unwrap();
        assert_eq!((req.destination.as_str(), req.sats), ("tark1x", 5));
    }

    #[tokio::test]
    async fn non_lnurl_destinations_pass_through() {
        assert_eq!(resolve_lnurl("tark1x", 5).await.unwrap(), "tark1x");
    }
}

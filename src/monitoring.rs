use anyhow::{anyhow, Context};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use log::{error, info};
use reqwest::Client;
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::AppState;

const DEFAULT_WINDOW_SECONDS: u64 = 3_600;
const DEFAULT_CHECK_INTERVAL_SECONDS: u64 = 60;
const DEFAULT_COOLDOWN_SECONDS: u64 = 3_600;

#[derive(Clone)]
pub struct PaymentAlertConfig {
    bot_token: String,
    chat_id: String,
    threshold_sats: u64,
    window: Duration,
    check_interval: Duration,
    cooldown: Duration,
}

#[derive(Clone)]
pub struct MonitoringHealth {
    alerts_configured: bool,
    analytics_writer_healthy: Arc<AtomicBool>,
    telegram_healthy: Arc<AtomicBool>,
}

impl MonitoringHealth {
    pub fn new(alerts_configured: bool) -> Self {
        Self {
            alerts_configured,
            analytics_writer_healthy: Arc::new(AtomicBool::new(false)),
            telegram_healthy: Arc::new(AtomicBool::new(!alerts_configured)),
        }
    }

    pub fn set_analytics_writer_healthy(&self, healthy: bool) {
        self.analytics_writer_healthy
            .store(healthy, Ordering::Relaxed);
    }

    pub fn analytics_writer_healthy(&self) -> bool {
        self.analytics_writer_healthy.load(Ordering::Relaxed)
    }

    fn set_telegram_healthy(&self, healthy: bool) {
        self.telegram_healthy.store(healthy, Ordering::Relaxed);
    }

    fn telegram_healthy(&self) -> bool {
        self.telegram_healthy.load(Ordering::Relaxed)
    }
}

#[derive(Serialize)]
struct MonitoringHealthResponse {
    status: &'static str,
    alerts_configured: bool,
    analytics_writer: &'static str,
    telegram: &'static str,
}

pub async fn monitoring_health_handler(Extension(state): Extension<AppState>) -> Response {
    let health = &state.monitoring_health;
    let writer_healthy = health.analytics_writer_healthy();
    let telegram_healthy = health.telegram_healthy();
    let healthy = writer_healthy && (!health.alerts_configured || telegram_healthy);

    let body = MonitoringHealthResponse {
        status: if healthy { "healthy" } else { "degraded" },
        alerts_configured: health.alerts_configured,
        analytics_writer: if writer_healthy {
            "healthy"
        } else {
            "degraded"
        },
        telegram: if !health.alerts_configured {
            "disabled"
        } else if telegram_healthy {
            "healthy"
        } else {
            "degraded"
        },
    };

    let status = if healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body)).into_response()
}

impl PaymentAlertConfig {
    /// Payment alerts are disabled when PAYMENT_ALERT_THRESHOLD_SATS is unset.
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        let Some(threshold) = optional_positive_u64("PAYMENT_ALERT_THRESHOLD_SATS")? else {
            return Ok(None);
        };

        let bot_token = env::var("TELEGRAM_BOT_TOKEN")
            .context("TELEGRAM_BOT_TOKEN is required when payment alerts are enabled")?;
        let chat_id = env::var("TELEGRAM_CHAT_ID")
            .context("TELEGRAM_CHAT_ID is required when payment alerts are enabled")?;

        if bot_token.trim().is_empty() {
            anyhow::bail!("TELEGRAM_BOT_TOKEN must not be empty");
        }
        if chat_id.trim().is_empty() {
            anyhow::bail!("TELEGRAM_CHAT_ID must not be empty");
        }

        Ok(Some(Self {
            bot_token,
            chat_id,
            threshold_sats: threshold,
            window: Duration::from_secs(positive_u64_or_default(
                "PAYMENT_ALERT_WINDOW_SECONDS",
                DEFAULT_WINDOW_SECONDS,
            )?),
            check_interval: Duration::from_secs(positive_u64_or_default(
                "PAYMENT_ALERT_CHECK_INTERVAL_SECONDS",
                DEFAULT_CHECK_INTERVAL_SECONDS,
            )?),
            cooldown: Duration::from_secs(positive_u64_or_default(
                "PAYMENT_ALERT_COOLDOWN_SECONDS",
                DEFAULT_COOLDOWN_SECONDS,
            )?),
        }))
    }
}

fn optional_positive_u64(name: &str) -> anyhow::Result<Option<u64>> {
    match env::var(name) {
        Ok(value) => {
            let parsed = value
                .parse::<u64>()
                .with_context(|| format!("{name} must be a positive integer"))?;
            if parsed == 0 {
                anyhow::bail!("{name} must be greater than zero");
            }
            Ok(Some(parsed))
        }
        Err(env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("failed to read {name}")),
    }
}

fn positive_u64_or_default(name: &str, default: u64) -> anyhow::Result<u64> {
    Ok(optional_positive_u64(name)?.unwrap_or(default))
}

#[derive(Debug, PartialEq)]
struct PaymentVolume {
    count: i64,
    total_sats: i64,
    by_type: Vec<(String, i64, i64)>,
}

async fn payment_volume(pool: &SqlitePool, window: Duration) -> Result<PaymentVolume, sqlx::Error> {
    let window_seconds = i64::try_from(window.as_secs()).unwrap_or(i64::MAX);
    let totals = sqlx::query(
        r#"SELECT COUNT(*) AS count, COALESCE(SUM(amount_sats), 0) AS total_sats
           FROM faucet_payments
           WHERE created_at >= strftime('%s', 'now') - $1
             AND payment_type != 'bolt11'"#,
    )
    .bind(window_seconds)
    .fetch_one(pool)
    .await?;

    let rows = sqlx::query(
        r#"SELECT payment_type, COUNT(*) AS count, COALESCE(SUM(amount_sats), 0) AS total_sats
           FROM faucet_payments
           WHERE created_at >= strftime('%s', 'now') - $1
             AND payment_type != 'bolt11'
           GROUP BY payment_type
           ORDER BY total_sats DESC"#,
    )
    .bind(window_seconds)
    .fetch_all(pool)
    .await?;

    Ok(PaymentVolume {
        count: totals.get("count"),
        total_sats: totals.get("total_sats"),
        by_type: rows
            .into_iter()
            .map(|row| {
                (
                    row.get("payment_type"),
                    row.get("count"),
                    row.get("total_sats"),
                )
            })
            .collect(),
    })
}

#[derive(Default)]
struct AlertState {
    alert_sent: bool,
    last_sent: Option<Instant>,
}

impl AlertState {
    fn must_send(&mut self, total_sats: u64, threshold_sats: u64, cooldown: Duration) -> bool {
        if total_sats < threshold_sats {
            self.alert_sent = false;
            self.last_sent = None;
            return false;
        }

        !self.alert_sent || self.last_sent.is_none_or(|sent| sent.elapsed() >= cooldown)
    }

    fn record_success(&mut self) {
        self.alert_sent = true;
        self.last_sent = Some(Instant::now());
    }
}

pub fn start_payment_volume_monitor(
    pool: SqlitePool,
    config: PaymentAlertConfig,
    health: MonitoringHealth,
) {
    tokio::spawn(async move {
        info!(
            "Payment alerts enabled: threshold={} sats, window={}s, cooldown={}s",
            config.threshold_sats,
            config.window.as_secs(),
            config.cooldown.as_secs()
        );

        let client = match Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
        {
            Ok(client) => client,
            Err(e) => {
                error!("Failed to create the Telegram client: {e}");
                return;
            }
        };
        let mut ticker = tokio::time::interval(config.check_interval);
        let mut state = AlertState::default();
        let mut startup_message_sent = false;
        let mut writer_alert_sent = false;

        loop {
            ticker.tick().await;

            let telegram_result = if startup_message_sent {
                check_telegram(&client, &config).await
            } else {
                send_telegram_message(&client, &config, &format_startup_message(&config)).await
            };
            match telegram_result {
                Ok(()) => {
                    health.set_telegram_healthy(true);
                    startup_message_sent = true;
                }
                Err(e) => {
                    health.set_telegram_healthy(false);
                    error!("Telegram alert readiness failed: {e:#}");
                    continue;
                }
            }

            let writer_healthy = health.analytics_writer_healthy();
            if !writer_healthy && !writer_alert_sent {
                let message = "⚠️ MutinyNet payment monitoring is degraded.\n\nThe analytics writer cannot store payment records. It will retry until the database recovers.";
                match send_telegram_message(&client, &config, message).await {
                    Ok(()) => writer_alert_sent = true,
                    Err(e) => {
                        health.set_telegram_healthy(false);
                        error!("Failed to send the analytics-writer alert: {e:#}");
                    }
                }
            } else if writer_healthy && writer_alert_sent {
                let message = "✅ MutinyNet payment monitoring recovered.\n\nThe analytics writer can store payment records again.";
                match send_telegram_message(&client, &config, message).await {
                    Ok(()) => writer_alert_sent = false,
                    Err(e) => {
                        health.set_telegram_healthy(false);
                        error!("Failed to send the analytics-writer recovery: {e:#}");
                    }
                }
            }

            let volume = match payment_volume(&pool, config.window).await {
                Ok(volume) => volume,
                Err(e) => {
                    error!("Failed to read payment volume for an alert: {e}");
                    continue;
                }
            };
            let total_sats = u64::try_from(volume.total_sats).unwrap_or(0);
            if !state.must_send(total_sats, config.threshold_sats, config.cooldown) {
                continue;
            }

            let message = format_alert(&volume, &config);
            match send_telegram_message(&client, &config, &message).await {
                Ok(()) => {
                    state.record_success();
                    info!("Sent a Telegram payment-volume alert");
                }
                Err(e) => {
                    health.set_telegram_healthy(false);
                    error!("Failed to send a Telegram payment-volume alert: {e:#}");
                }
            }
        }
    });
}

fn format_startup_message(config: &PaymentAlertConfig) -> String {
    format!(
        "✅ MutinyNet faucet payment alerts started.\n\nThreshold: {} sats in {} minutes.\nRepeat cooldown: {} minutes.",
        format_number(config.threshold_sats),
        config.window.as_secs().div_ceil(60),
        config.cooldown.as_secs().div_ceil(60)
    )
}

fn format_alert(volume: &PaymentVolume, config: &PaymentAlertConfig) -> String {
    let mut message = format!(
        "🚨 MutinyNet faucet payment-volume alert\n\n{} sats across {} outgoing payments in the last {} minutes.\nThreshold: {} sats.",
        format_number(volume.total_sats),
        format_number(volume.count),
        config.window.as_secs().div_ceil(60),
        format_number(config.threshold_sats)
    );

    if !volume.by_type.is_empty() {
        message.push_str("\n\nBy type:");
        for (payment_type, count, total_sats) in &volume.by_type {
            message.push_str(&format!(
                "\n- {payment_type}: {} sats ({} payments)",
                format_number(*total_sats),
                format_number(*count)
            ));
        }
    }
    message
}

fn format_number(value: impl Into<i128>) -> String {
    let value = value.into();
    let negative = value < 0;
    let digits = value.abs().to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            formatted.push(',');
        }
        formatted.push(character);
    }
    if negative {
        formatted.insert(0, '-');
    }
    formatted
}

async fn send_telegram_message(
    client: &Client,
    config: &PaymentAlertConfig,
    text: &str,
) -> anyhow::Result<()> {
    telegram_request(
        client,
        config,
        "sendMessage",
        serde_json::json!({
            "chat_id": config.chat_id,
            "text": text
        }),
    )
    .await
}

async fn check_telegram(client: &Client, config: &PaymentAlertConfig) -> anyhow::Result<()> {
    telegram_request(
        client,
        config,
        "getChat",
        serde_json::json!({ "chat_id": config.chat_id }),
    )
    .await
}

async fn telegram_request(
    client: &Client,
    config: &PaymentAlertConfig,
    method: &str,
    body: serde_json::Value,
) -> anyhow::Result<()> {
    let url = format!("https://api.telegram.org/bot{}/{method}", config.bot_token);
    let mut response = client
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|error| anyhow!("Telegram request failed: {}", error.without_url()))?;

    if response.status().is_success() {
        return Ok(());
    }

    let status = response.status();
    let mut bytes = Vec::with_capacity(512);
    while bytes.len() < 512 {
        let Some(chunk) = response.chunk().await.map_err(|error| {
            anyhow!(
                "Failed to read the Telegram response: {}",
                error.without_url()
            )
        })?
        else {
            break;
        };
        let remaining = 512 - bytes.len();
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    let body = sanitize_telegram_error(&bytes, config);
    Err(anyhow!("Telegram returned {status}: {body}"))
}

fn sanitize_telegram_error(bytes: &[u8], config: &PaymentAlertConfig) -> String {
    String::from_utf8_lossy(bytes)
        .replace(&config.bot_token, "[redacted]")
        .replace(&config.chat_id, "[redacted]")
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alert_rearms_after_volume_decreases() {
        let mut state = AlertState::default();
        let cooldown = Duration::from_secs(60);

        assert!(state.must_send(100, 100, cooldown));
        state.record_success();
        assert!(!state.must_send(100, 100, cooldown));
        assert!(!state.must_send(99, 100, cooldown));
        assert!(state.must_send(100, 100, cooldown));
    }

    #[tokio::test]
    async fn volume_excludes_generated_invoices() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE faucet_payments (created_at INTEGER NOT NULL, payment_type TEXT NOT NULL, amount_sats INTEGER NOT NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO faucet_payments VALUES (strftime('%s', 'now'), 'lightning', 200), (strftime('%s', 'now'), 'bolt11', 900)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let volume = payment_volume(&pool, Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(volume.count, 1);
        assert_eq!(volume.total_sats, 200);
        assert_eq!(volume.by_type, vec![("lightning".to_string(), 1, 200)]);
    }

    #[test]
    fn alert_contains_grouped_payment_volume() {
        let config = PaymentAlertConfig {
            bot_token: "token".to_string(),
            chat_id: "chat".to_string(),
            threshold_sats: 1_000_000,
            window: Duration::from_secs(3_600),
            check_interval: Duration::from_secs(60),
            cooldown: Duration::from_secs(3_600),
        };
        let volume = PaymentVolume {
            count: 12,
            total_sats: 1_234_567,
            by_type: vec![("onchain".to_string(), 12, 1_234_567)],
        };

        let alert = format_alert(&volume, &config);
        assert!(alert.contains("1,234,567 sats across 12 outgoing payments"));
        assert!(alert.contains("onchain: 1,234,567 sats (12 payments)"));
    }

    #[test]
    fn telegram_errors_are_redacted_and_single_line() {
        let config = PaymentAlertConfig {
            bot_token: "secret-token".to_string(),
            chat_id: "chat-123".to_string(),
            threshold_sats: 1,
            window: Duration::from_secs(60),
            check_interval: Duration::from_secs(60),
            cooldown: Duration::from_secs(60),
        };

        let error = sanitize_telegram_error(b"bad secret-token\nfor chat-123\ttry again", &config);
        assert_eq!(error, "bad [redacted] for [redacted] try again");
        assert!(!error.chars().any(char::is_control));
    }
}

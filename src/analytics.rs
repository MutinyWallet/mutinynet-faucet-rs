use axum::extract::Query;
use axum::{Extension, Json};
use log::error;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{Row, SqlitePool};
use std::collections::HashMap;
use tokio::sync::mpsc;

use crate::AppError;

pub async fn init_analytics_db(path: &str) -> anyhow::Result<SqlitePool> {
    let url = format!("sqlite:{path}?mode=rwc");
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await?;

    // WAL mode: allows concurrent reads while writing
    sqlx::query("PRAGMA journal_mode=WAL")
        .execute(&pool)
        .await?;
    // Wait up to 5s for the write lock instead of failing immediately
    sqlx::query("PRAGMA busy_timeout=5000")
        .execute(&pool)
        .await?;
    // NORMAL sync is fine for analytics — tolerate losing last write on crash
    sqlx::query("PRAGMA synchronous=NORMAL")
        .execute(&pool)
        .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS faucet_payments (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
            payment_type TEXT NOT NULL,
            amount_sats INTEGER NOT NULL,
            username TEXT,
            ip_address TEXT NOT NULL,
            destination TEXT
        )
        "#,
    )
    .execute(&pool)
    .await?;

    // Composite index for timeseries queries (filter by created_at, group by payment_type)
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_faucet_payments_created_type ON faucet_payments (created_at, payment_type)",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_faucet_payments_username ON faucet_payments (username)",
    )
    .execute(&pool)
    .await?;

    Ok(pool)
}

pub struct AnalyticsPayment {
    payment_type: String,
    amount_sats: i64,
    username: Option<String>,
    ip_address: String,
    destination: Option<String>,
}

/// Starts a background writer that batches inserts to reduce SQLite write contention.
/// Returns a sender that `record_payment` uses to enqueue writes.
pub fn start_write_batcher(pool: SqlitePool) -> mpsc::UnboundedSender<AnalyticsPayment> {
    let (tx, mut rx) = mpsc::unbounded_channel::<AnalyticsPayment>();

    tokio::spawn(async move {
        // Collect up to 64 records or 500ms, whichever comes first
        let mut buf: Vec<AnalyticsPayment> = Vec::with_capacity(64);

        loop {
            // Wait for the first record (blocks until one arrives or channel closes)
            let first = rx.recv().await;
            let Some(record) = first else { break };
            buf.push(record);

            // Drain any additional records that are already queued, up to 64
            while buf.len() < 64 {
                match rx.try_recv() {
                    Ok(record) => buf.push(record),
                    Err(_) => break,
                }
            }

            // Batch insert in a single transaction
            if let Err(e) = flush_batch(&pool, &buf).await {
                error!(
                    "Failed to flush analytics batch ({} records): {e}",
                    buf.len()
                );
            }
            buf.clear();
        }
    });

    tx
}

async fn flush_batch(pool: &SqlitePool, records: &[AnalyticsPayment]) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    for r in records {
        sqlx::query(
            "INSERT INTO faucet_payments (payment_type, amount_sats, username, ip_address, destination) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&r.payment_type)
        .bind(r.amount_sats)
        .bind(&r.username)
        .bind(&r.ip_address)
        .bind(&r.destination)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub fn record_payment(
    tx: &mpsc::UnboundedSender<AnalyticsPayment>,
    payment_type: &str,
    amount_sats: u64,
    username: Option<&str>,
    ip_address: &str,
    destination: Option<&str>,
) {
    let _ = tx.send(AnalyticsPayment {
        payment_type: payment_type.to_string(),
        amount_sats: amount_sats as i64,
        username: username.map(|s| s.to_string()),
        ip_address: ip_address.to_string(),
        destination: destination.map(|s| s.to_string()),
    });
}

fn get_pool(state: &crate::AppState) -> Result<&SqlitePool, AppError> {
    state
        .analytics_db
        .as_ref()
        .ok_or_else(|| AppError::new("Analytics not enabled"))
}

// -- Summary --

#[derive(Deserialize)]
pub struct SummaryParams {
    /// Number of hours to look back (default: 24)
    pub hours: Option<i64>,
    /// Filter to a specific payment type
    pub payment_type: Option<String>,
}

pub async fn analytics_summary(
    Extension(state): Extension<crate::AppState>,
    Query(params): Query<SummaryParams>,
) -> Result<Json<Value>, AppError> {
    let pool = get_pool(&state)?;

    let hours = params.hours.unwrap_or(24);
    let cutoff = chrono::Utc::now().timestamp() - (hours * 3600);

    let (type_filter, bind_type) = type_filter_clause(&params.payment_type, 2);

    let query_str = format!(
        r#"
        SELECT payment_type, COUNT(*) as count, COALESCE(SUM(amount_sats), 0) as total_sats
        FROM faucet_payments
        WHERE created_at > $1 {type_filter}
        GROUP BY payment_type
        ORDER BY total_sats DESC
        "#,
    );

    let mut q = sqlx::query(&query_str).bind(cutoff);
    if let Some(ref t) = bind_type {
        q = q.bind(t);
    }
    let rows = q.fetch_all(pool).await?;

    let by_type: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "payment_type": row.get::<String, _>("payment_type"),
                "count": row.get::<i64, _>("count"),
                "total_sats": row.get::<i64, _>("total_sats"),
            })
        })
        .collect();

    let total_count: i64 = rows.iter().map(|r| r.get::<i64, _>("count")).sum();
    let total_sats: i64 = rows.iter().map(|r| r.get::<i64, _>("total_sats")).sum();

    // Unique users
    let unique_query = format!(
        r#"
        SELECT COUNT(DISTINCT COALESCE(username, ip_address)) as unique_users
        FROM faucet_payments
        WHERE created_at > $1 {type_filter}
        "#,
    );
    let mut q = sqlx::query(&unique_query).bind(cutoff);
    if let Some(ref t) = bind_type {
        q = q.bind(t);
    }
    let unique_row = q.fetch_one(pool).await?;
    let unique_users: i64 = unique_row.get("unique_users");

    let avg_sats = if total_count > 0 {
        total_sats / total_count
    } else {
        0
    };

    Ok(Json(json!({
        "hours": hours,
        "total_count": total_count,
        "total_sats": total_sats,
        "unique_users": unique_users,
        "avg_sats": avg_sats,
        "by_type": by_type,
    })))
}

// -- Timeseries --

#[derive(Deserialize)]
pub struct TimeseriesParams {
    /// Number of hours to look back (default: 24)
    pub hours: Option<i64>,
    /// Bucket interval: "hour" or "day" (default: "hour")
    pub interval: Option<String>,
    /// Filter to a specific payment type
    pub payment_type: Option<String>,
}

pub async fn analytics_timeseries(
    Extension(state): Extension<crate::AppState>,
    Query(params): Query<TimeseriesParams>,
) -> Result<Json<Value>, AppError> {
    let pool = get_pool(&state)?;

    let hours = params.hours.unwrap_or(24);
    let cutoff = chrono::Utc::now().timestamp() - (hours * 3600);
    let interval = params.interval.as_deref().unwrap_or("hour");

    let format_str = match interval {
        "day" => "%Y-%m-%d",
        _ => "%Y-%m-%dT%H:00:00Z",
    };

    let (type_filter, bind_type) = type_filter_clause(&params.payment_type, 3);

    // Query with per-type breakdown within each bucket
    let query_str = format!(
        r#"
        SELECT strftime($1, created_at, 'unixepoch') as bucket,
               payment_type,
               COUNT(*) as count,
               COALESCE(SUM(amount_sats), 0) as total_sats
        FROM faucet_payments
        WHERE created_at > $2 {type_filter}
        GROUP BY bucket, payment_type
        ORDER BY bucket ASC
        "#,
    );

    let mut q = sqlx::query(&query_str).bind(format_str).bind(cutoff);
    if let Some(ref t) = bind_type {
        q = q.bind(t);
    }
    let rows = q.fetch_all(pool).await?;

    // Group into buckets with per-type breakdown
    let mut bucket_map: HashMap<String, BucketData> = HashMap::new();
    let mut bucket_order: Vec<String> = Vec::new();

    for row in &rows {
        let bucket: String = row.get("bucket");
        let payment_type: String = row.get("payment_type");
        let count: i64 = row.get("count");
        let total_sats: i64 = row.get("total_sats");

        let entry = bucket_map.entry(bucket.clone()).or_insert_with(|| {
            bucket_order.push(bucket.clone());
            BucketData::default()
        });
        entry.count += count;
        entry.total_sats += total_sats;
        entry.by_type.push(json!({
            "payment_type": payment_type,
            "count": count,
            "total_sats": total_sats,
        }));
    }

    let buckets: Vec<Value> = bucket_order
        .iter()
        .map(|time| {
            let data = &bucket_map[time];
            json!({
                "time": time,
                "count": data.count,
                "total_sats": data.total_sats,
                "by_type": data.by_type,
            })
        })
        .collect();

    Ok(Json(json!({
        "hours": hours,
        "interval": interval,
        "buckets": buckets,
    })))
}

#[derive(Default)]
struct BucketData {
    count: i64,
    total_sats: i64,
    by_type: Vec<Value>,
}

// -- Users --

#[derive(Deserialize)]
pub struct UsersParams {
    /// Number of hours to look back (default: 24)
    pub hours: Option<i64>,
    /// Max users to return (default: 50)
    pub limit: Option<i64>,
    /// Filter to a specific payment type
    pub payment_type: Option<String>,
}

pub async fn analytics_users(
    Extension(state): Extension<crate::AppState>,
    Query(params): Query<UsersParams>,
) -> Result<Json<Value>, AppError> {
    let pool = get_pool(&state)?;

    let hours = params.hours.unwrap_or(24);
    let limit = params.limit.unwrap_or(50);
    let cutoff = chrono::Utc::now().timestamp() - (hours * 3600);

    let (type_filter, bind_type) = type_filter_clause(&params.payment_type, 3);

    // Get top users
    let top_query = format!(
        r#"
        SELECT COALESCE(username, ip_address) as user_id,
               COUNT(*) as count,
               COALESCE(SUM(amount_sats), 0) as total_sats,
               MAX(created_at) as last_payment
        FROM faucet_payments
        WHERE created_at > $1 {type_filter}
        GROUP BY user_id
        ORDER BY total_sats DESC
        LIMIT $2
        "#,
    );

    let mut q = sqlx::query(&top_query).bind(cutoff);
    if let Some(ref t) = bind_type {
        q = q.bind(t);
    }
    let top_rows = q.bind(limit).fetch_all(pool).await?;

    let user_ids: Vec<String> = top_rows
        .iter()
        .map(|r| r.get::<String, _>("user_id"))
        .collect();

    // Get per-type breakdown for those users
    let type_breakdown = if !user_ids.is_empty() {
        let placeholders: Vec<String> =
            (0..user_ids.len()).map(|i| format!("${}", i + 2)).collect();
        let in_clause = placeholders.join(",");
        let breakdown_query = format!(
            r#"
            SELECT COALESCE(username, ip_address) as user_id,
                   payment_type,
                   COUNT(*) as count,
                   COALESCE(SUM(amount_sats), 0) as total_sats
            FROM faucet_payments
            WHERE created_at > $1 AND COALESCE(username, ip_address) IN ({in_clause})
            GROUP BY user_id, payment_type
            "#,
        );

        let mut q = sqlx::query(&breakdown_query).bind(cutoff);
        for uid in &user_ids {
            q = q.bind(uid);
        }
        let breakdown_rows = q.fetch_all(pool).await?;

        let mut map: HashMap<String, Vec<Value>> = HashMap::new();
        for row in &breakdown_rows {
            let uid: String = row.get("user_id");
            map.entry(uid).or_default().push(json!({
                "payment_type": row.get::<String, _>("payment_type"),
                "count": row.get::<i64, _>("count"),
                "total_sats": row.get::<i64, _>("total_sats"),
            }));
        }
        map
    } else {
        HashMap::new()
    };

    let users: Vec<Value> = top_rows
        .iter()
        .map(|row| {
            let user_id = row.get::<String, _>("user_id");
            let by_type = type_breakdown.get(&user_id).cloned().unwrap_or_default();
            json!({
                "user": user_id,
                "count": row.get::<i64, _>("count"),
                "total_sats": row.get::<i64, _>("total_sats"),
                "last_payment": row.get::<i64, _>("last_payment"),
                "by_type": by_type,
            })
        })
        .collect();

    Ok(Json(json!({
        "hours": hours,
        "users": users,
    })))
}

// -- Recent activity --

#[derive(Deserialize)]
pub struct RecentParams {
    /// Max payments to return (default: 50)
    pub limit: Option<i64>,
    /// Filter to a specific payment type
    pub payment_type: Option<String>,
}

pub async fn analytics_recent(
    Extension(state): Extension<crate::AppState>,
    Query(params): Query<RecentParams>,
) -> Result<Json<Value>, AppError> {
    let pool = get_pool(&state)?;

    let limit = params.limit.unwrap_or(50);
    let (type_filter, bind_type) = type_filter_clause(&params.payment_type, 2);

    let query_str = format!(
        r#"
        SELECT id, created_at, payment_type, amount_sats,
               COALESCE(username, ip_address) as user_id, destination
        FROM faucet_payments
        WHERE 1=1 {type_filter}
        ORDER BY created_at DESC
        LIMIT $1
        "#,
    );

    let mut q = sqlx::query(&query_str).bind(limit);
    if let Some(ref t) = bind_type {
        q = q.bind(t);
    }
    let rows = q.fetch_all(pool).await?;

    let payments: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.get::<i64, _>("id"),
                "created_at": row.get::<i64, _>("created_at"),
                "payment_type": row.get::<String, _>("payment_type"),
                "amount_sats": row.get::<i64, _>("amount_sats"),
                "user": row.get::<String, _>("user_id"),
                "destination": row.get::<Option<String>, _>("destination"),
            })
        })
        .collect();

    Ok(Json(json!({
        "payments": payments,
    })))
}

// -- Domains --

#[derive(Deserialize)]
pub struct DomainsParams {
    /// Number of hours to look back (default: 24)
    pub hours: Option<i64>,
    /// Max domains to return (default: 50)
    pub limit: Option<i64>,
    /// Filter to a specific payment type
    pub payment_type: Option<String>,
}

pub async fn analytics_domains(
    Extension(state): Extension<crate::AppState>,
    Query(params): Query<DomainsParams>,
) -> Result<Json<Value>, AppError> {
    let pool = get_pool(&state)?;

    let hours = params.hours.unwrap_or(24);
    let limit = params.limit.unwrap_or(50);
    let cutoff = chrono::Utc::now().timestamp() - (hours * 3600);

    let (type_filter, bind_type) = type_filter_clause(&params.payment_type, 3);

    // Extract domain from email usernames (everything after @)
    // Exclude non-email usernames (l402:*, IPs, nostr pubkeys)
    let query_str = format!(
        r#"
        SELECT LOWER(SUBSTR(username, INSTR(username, '@') + 1)) as domain,
               COUNT(*) as count,
               COALESCE(SUM(amount_sats), 0) as total_sats,
               COUNT(DISTINCT username) as unique_users
        FROM faucet_payments
        WHERE created_at > $1
          AND username IS NOT NULL
          AND username LIKE '%@%'
          {type_filter}
        GROUP BY domain
        ORDER BY total_sats DESC
        LIMIT $2
        "#,
    );

    let mut q = sqlx::query(&query_str).bind(cutoff);
    if let Some(ref t) = bind_type {
        q = q.bind(t);
    }
    let rows = q.bind(limit).fetch_all(pool).await?;

    let domains: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "domain": row.get::<String, _>("domain"),
                "count": row.get::<i64, _>("count"),
                "total_sats": row.get::<i64, _>("total_sats"),
                "unique_users": row.get::<i64, _>("unique_users"),
            })
        })
        .collect();

    let total_count: i64 = rows.iter().map(|r| r.get::<i64, _>("count")).sum();
    let total_sats: i64 = rows.iter().map(|r| r.get::<i64, _>("total_sats")).sum();

    Ok(Json(json!({
        "hours": hours,
        "total_count": total_count,
        "total_sats": total_sats,
        "domains": domains,
    })))
}

/// Returns a SQL fragment like `AND payment_type = $N` and the value to bind,
/// or empty string + None if no filter is requested.
fn type_filter_clause(payment_type: &Option<String>, param_index: u8) -> (String, Option<String>) {
    match payment_type {
        Some(t) if !t.is_empty() => (
            format!("AND payment_type = ${param_index}"),
            Some(t.clone()),
        ),
        _ => (String::new(), None),
    }
}

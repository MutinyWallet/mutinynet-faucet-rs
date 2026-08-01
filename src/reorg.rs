use anyhow::{anyhow, Result};
use bitcoincore_rpc::RpcApi;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{sleep, Duration};
use tonic_openssl_lnd::lnrpc;

use crate::auth::AuthUser;
use crate::AppState;

#[derive(Deserialize)]
pub struct ReorgInvoiceRequest {
    pub blocks: u8,
}

#[derive(Serialize)]
pub struct ReorgInvoiceResponse {
    pub invoice: String,
    pub payment_hash: String,
    pub amount_sats: u64,
    pub blocks: u8,
}

#[derive(Debug)]
struct PendingReorg {
    payment_hash: String,
    blocks: u8,
    username: String,
}

enum InvalidateAttempt {
    /// No irreversible RPC was attempted, so the reservation can be released.
    NotStarted(anyhow::Error),
    /// `invalidateblock` was sent. An error is ambiguous because Bitcoin Core
    /// may have applied it before the connection failed.
    Started {
        target_height: u64,
        target_hash: String,
        result: Result<()>,
    },
}

/// Return the oldest block that must be invalidated to remove `blocks`
/// blocks from the active tip. Never return the genesis block.
fn reorg_target_height(current_height: u64, blocks: u8) -> Option<u64> {
    let blocks = u64::from(blocks);
    if blocks == 0 || current_height < blocks {
        return None;
    }
    Some(current_height - blocks + 1)
}

/// Initialize the reorg database
pub async fn init_reorg_db(db_path: &str) -> Result<SqlitePool> {
    // Create parent directories if they don't exist
    if let Some(parent) = std::path::Path::new(db_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Create database file if it doesn't exist
    if !std::path::Path::new(db_path).exists() {
        std::fs::File::create(db_path)?;
    }

    let pool = SqlitePool::connect(&format!("sqlite:{}", db_path)).await?;

    // WAL mode allows concurrent reads while writing; busy_timeout avoids
    // immediate "database is locked" failures under contention.
    sqlx::query("PRAGMA journal_mode=WAL")
        .execute(&pool)
        .await?;
    sqlx::query("PRAGMA busy_timeout=5000")
        .execute(&pool)
        .await?;

    // Run schema
    let schema = include_str!("../schema.sql");
    sqlx::query(schema).execute(&pool).await?;

    info!("Reorg database initialized at {}", db_path);
    Ok(pool)
}

/// Check if cooldown allows a new reorg
async fn check_cooldown(pool: &SqlitePool, cooldown_seconds: u64) -> Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;

    let row: (i64,) =
        sqlx::query_as("SELECT last_reorg_timestamp FROM reorg_cooldown WHERE id = 1")
            .fetch_one(pool)
            .await?;

    let last_reorg = row.0;
    let elapsed = now - last_reorg;

    if elapsed < cooldown_seconds as i64 {
        let remaining = cooldown_seconds as i64 - elapsed;
        return Err(anyhow!(
            "Reorg cooldown active. Please wait {remaining} seconds"
        ));
    }

    Ok(())
}

/// Store a pending reorg in the database
async fn store_pending_reorg(
    pool: &SqlitePool,
    payment_hash: &str,
    blocks: u8,
    username: &str,
) -> Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;

    sqlx::query(
        "INSERT INTO reorgs (payment_hash, blocks, username, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(payment_hash)
    .bind(blocks as i64)
    .bind(username)
    .bind(now)
    .execute(pool)
    .await?;

    Ok(())
}

/// Get a pending reorg by payment hash
async fn get_pending_reorg(pool: &SqlitePool, payment_hash: &str) -> Result<Option<PendingReorg>> {
    let result = sqlx::query_as::<_, (String, i64, String)>(
        "SELECT payment_hash, blocks, username FROM reorgs WHERE payment_hash = ? AND status = 'pending'",
    )
    .bind(payment_hash)
    .fetch_optional(pool)
    .await?;

    Ok(result.map(|(payment_hash, blocks, username)| PendingReorg {
        payment_hash,
        blocks: blocks as u8,
        username,
    }))
}

/// Get all pending reorgs
async fn get_all_reorgs(pool: &SqlitePool) -> Result<Vec<PendingReorg>> {
    let results = sqlx::query_as::<_, (String, i64, String)>(
        "SELECT payment_hash, blocks, username FROM reorgs WHERE status = 'pending'",
    )
    .fetch_all(pool)
    .await?;

    Ok(results
        .into_iter()
        .map(|(payment_hash, blocks, username)| PendingReorg {
            payment_hash,
            blocks: blocks as u8,
            username,
        })
        .collect())
}

/// Update reorg status (for accounting - never delete records)
async fn update_reorg_status(
    pool: &SqlitePool,
    payment_hash: &str,
    status: &str,
    executed_at: Option<i64>,
    invalidated_block_height: Option<i64>,
    invalidated_block_hash: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE reorgs SET status = ?, executed_at = ?, invalidated_block_height = ?, invalidated_block_hash = ? WHERE payment_hash = ?"
    )
    .bind(status)
    .bind(executed_at)
    .bind(invalidated_block_height)
    .bind(invalidated_block_hash)
    .bind(payment_hash)
    .execute(pool)
    .await?;

    Ok(())
}

/// Generate a reorg invoice (stores in DB, waits for payment via subscription)
pub async fn generate_reorg_invoice(
    state: &AppState,
    user: &AuthUser,
    request: ReorgInvoiceRequest,
) -> Result<ReorgInvoiceResponse> {
    // Validate feature enabled
    if !state.reorg_config.enabled {
        return Err(anyhow!("Reorg functionality is not enabled"));
    }

    // Validate blocks parameter (1-5 range)
    if request.blocks < 1 || request.blocks > 5 {
        return Err(anyhow!("Blocks must be between 1 and 5"));
    }

    // Keep the availability check, LND invoice creation, and pending-row
    // insert in one process-wide critical section. Without this, concurrent
    // callers can both observe no pending reorg before either row is stored.
    let _operation_guard = state.reorg_operation_lock.lock().await;

    // Get pricing
    let amount_sats = state
        .reorg_config
        .pricing
        .get(&request.blocks)
        .ok_or_else(|| anyhow!("Invalid blocks value"))?;

    // Check cooldown from database
    let pool = state
        .reorg_db
        .as_ref()
        .ok_or_else(|| anyhow!("Reorg database not initialized"))?;

    check_cooldown(pool, state.reorg_config.cooldown_seconds).await?;

    // Reject new invoices while a reorg is pending or executing: only one
    // reorg executes per cooldown window, and extra paid invoices would be
    // marked skipped, losing the buyer's (mainnet) money.
    let busy: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM reorgs WHERE status IN ('pending', 'executing', 'uncertain')",
    )
    .fetch_one(pool)
    .await?;
    if busy.0 > 0 {
        return Err(anyhow!(
            "a reorg is already pending; try again after it executes"
        ));
    }

    // Generate invoice on mainnet LND
    let mainnet_client = state
        .mainnet_lightning_client
        .as_ref()
        .ok_or_else(|| anyhow!("Mainnet LND client not configured"))?;

    let blocks_word = if request.blocks == 1 {
        "block"
    } else {
        "blocks"
    };
    let memo = format!(
        "Mutinynet Reorg: {} {} for user {}",
        request.blocks, blocks_word, user.username
    );

    let add_invoice_request = lnrpc::Invoice {
        memo,
        value: *amount_sats as i64,
        expiry: 600, // 10 minutes
        ..Default::default()
    };

    let response = mainnet_client
        .clone()
        .add_invoice(add_invoice_request)
        .await?
        .into_inner();

    let payment_hash = hex::encode(&response.r_hash);

    // Store in database
    store_pending_reorg(pool, &payment_hash, request.blocks, &user.username).await?;

    info!(
        "Generated reorg invoice for user {}: {} blocks, payment_hash: {}",
        user.username, request.blocks, payment_hash
    );

    Ok(ReorgInvoiceResponse {
        invoice: response.payment_request,
        payment_hash,
        amount_sats: *amount_sats,
        blocks: request.blocks,
    })
}

/// Atomically advance the cooldown and mark the reorg as executing.
/// Fails if the reorg is no longer pending (already handled elsewhere).
/// Returns the previous cooldown timestamp for rollback.
async fn reserve_reorg_execution(pool: &SqlitePool, payment_hash: &str, now: i64) -> Result<i64> {
    let prev: (i64,) =
        sqlx::query_as("SELECT last_reorg_timestamp FROM reorg_cooldown WHERE id = 1")
            .fetch_one(pool)
            .await?;

    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE reorg_cooldown SET last_reorg_timestamp = ? WHERE id = 1")
        .bind(now)
        .execute(&mut *tx)
        .await?;
    let res = sqlx::query(
        "UPDATE reorgs SET status = 'executing' WHERE payment_hash = ? AND status = 'pending'",
    )
    .bind(payment_hash)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() == 0 {
        tx.rollback().await?;
        return Err(anyhow!("reorg is no longer pending"));
    }
    tx.commit().await?;
    Ok(prev.0)
}

/// Roll back a reserved execution when the RPC calls failed before
/// invalidate_block, so the reorg stays pending and can be retried.
async fn rollback_reorg_execution(
    pool: &SqlitePool,
    payment_hash: &str,
    prev_cooldown: i64,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE reorg_cooldown SET last_reorg_timestamp = ? WHERE id = 1")
        .bind(prev_cooldown)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE reorgs SET status = 'pending' WHERE payment_hash = ? AND status = 'executing'",
    )
    .bind(payment_hash)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Execute a reorg (internal function called when invoice is paid)
async fn execute_reorg_internal(state: &AppState, pending_reorg: &PendingReorg) -> Result<()> {
    // Serialize the cooldown check and irreversible RPC with invoice creation
    // and any other execution attempt in this process.
    let _operation_guard = state.reorg_operation_lock.lock().await;

    let pool = state
        .reorg_db
        .as_ref()
        .ok_or_else(|| anyhow!("Reorg database not initialized"))?;

    // Double-check cooldown
    check_cooldown(pool, state.reorg_config.cooldown_seconds).await?;

    // Reserve the execution BEFORE the irreversible invalidate_block:
    // advance the cooldown and mark the row as executing in one transaction.
    // If the process crashes after invalidate_block, the row stays in
    // 'executing' and is never re-executed (startup only scans 'pending').
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let prev_cooldown = reserve_reorg_execution(pool, &pending_reorg.payment_hash, now).await?;

    // Get Bitcoin Core RPC client
    let bitcoin_rpc = state
        .bitcoin_rpc
        .as_ref()
        .ok_or_else(|| anyhow!("Bitcoin Core RPC not configured"))?
        .clone();

    // The RPC client is synchronous; keep it off the async worker threads.
    let blocks = pending_reorg.blocks;
    let attempt = tokio::task::spawn_blocking(move || {
        let prepared = (|| -> Result<_> {
            let current_height = bitcoin_rpc.get_block_count()?;

            let target_height = reorg_target_height(current_height, blocks).ok_or_else(|| {
                anyhow!(
                    "Not enough blocks in chain to reorg. Current height: {}, requested: {}",
                    current_height,
                    blocks
                )
            })?;
            let target_block_hash = bitcoin_rpc.get_block_hash(target_height)?;
            Ok((target_height, target_block_hash))
        })();

        match prepared {
            Err(e) => InvalidateAttempt::NotStarted(e),
            Ok((target_height, target_block_hash)) => {
                let target_hash = target_block_hash.to_string();
                let result = bitcoin_rpc
                    .invalidate_block(&target_block_hash)
                    .map_err(anyhow::Error::from);
                InvalidateAttempt::Started {
                    target_height,
                    target_hash,
                    result,
                }
            }
        }
    })
    .await
    .map_err(|e| anyhow!("reorg RPC task failed after reservation: {e}"))?;

    let (target_height, target_block_hash_str) = match attempt {
        InvalidateAttempt::NotStarted(e) => {
            // Preparation failed before invalidateblock was sent, so retrying
            // is safe after restoring the reservation.
            if let Err(re) =
                rollback_reorg_execution(pool, &pending_reorg.payment_hash, prev_cooldown).await
            {
                error!("failed to roll back reorg reservation: {re}");
            }
            return Err(e);
        }
        InvalidateAttempt::Started {
            target_height,
            target_hash,
            result: Ok(()),
        } => (target_height, target_hash),
        InvalidateAttempt::Started {
            target_height,
            target_hash,
            result: Err(e),
        } => {
            // Do not retry an ambiguous transport/server error: Bitcoin Core
            // may have invalidated the block before its response was lost.
            if let Err(status_error) = update_reorg_status(
                pool,
                &pending_reorg.payment_hash,
                "uncertain",
                Some(now),
                Some(target_height as i64),
                Some(&target_hash),
            )
            .await
            {
                error!("failed to mark ambiguous reorg as uncertain: {status_error}");
            }
            return Err(anyhow!(
                "invalidateblock result is uncertain for {target_hash}: {e}"
            ));
        }
    };

    // Mark the reorg as executed (cooldown was already advanced by the
    // reservation).
    update_reorg_status(
        pool,
        &pending_reorg.payment_hash,
        "executed",
        Some(now),
        Some(target_height as i64),
        Some(&target_block_hash_str),
    )
    .await?;

    info!(
        "Reorg executed for user {}: {} blocks invalidated, invalidated block {} at height {}",
        pending_reorg.username, pending_reorg.blocks, target_block_hash_str, target_height
    );

    Ok(())
}

/// Background task that subscribes to LND invoice updates and executes reorgs
pub async fn start_reorg_invoice_listener(state: AppState) {
    info!("Starting reorg invoice listener");

    loop {
        if let Err(e) = run_invoice_listener(&state).await {
            error!(
                "Reorg invoice listener error: {}. Restarting in 10 seconds...",
                e
            );
            sleep(Duration::from_secs(10)).await;
        }
    }
}

async fn run_invoice_listener(state: &AppState) -> Result<()> {
    // Check if feature is enabled
    if !state.reorg_config.enabled {
        warn!("Reorg feature is disabled, invoice listener not starting");
        sleep(Duration::from_secs(60)).await;
        return Ok(());
    }

    let mainnet_client = state
        .mainnet_lightning_client
        .as_ref()
        .ok_or_else(|| anyhow!("Mainnet LND client not configured"))?;

    let pool = state
        .reorg_db
        .as_ref()
        .ok_or_else(|| anyhow!("Reorg database not initialized"))?;

    // On startup, check all pending reorgs for settled invoices
    info!("Checking pending reorgs for settled invoices...");
    let pending = get_all_reorgs(pool).await?;

    // Find all settled invoices
    let mut settled_reorgs = Vec::new();

    for pending_reorg in pending {
        let payment_hash = hex::decode(&pending_reorg.payment_hash)?;

        // Check if invoice is settled
        let lookup_request = lnrpc::PaymentHash {
            r_hash: payment_hash.clone(),
            ..Default::default()
        };

        match mainnet_client.clone().lookup_invoice(lookup_request).await {
            Ok(response) => {
                let invoice = response.into_inner();
                if invoice.state == lnrpc::invoice::InvoiceState::Settled as i32 {
                    info!(
                        "Found settled invoice for pending reorg: {} blocks for user {}",
                        pending_reorg.blocks, pending_reorg.username
                    );
                    settled_reorgs.push(pending_reorg);
                } else if invoice.state == lnrpc::invoice::InvoiceState::Canceled as i32 {
                    info!(
                        "Found expired invoice for pending reorg: {} blocks for user {} (payment_hash: {})",
                        pending_reorg.blocks, pending_reorg.username, pending_reorg.payment_hash
                    );
                    if let Err(e) = update_reorg_status(
                        pool,
                        &pending_reorg.payment_hash,
                        "expired",
                        None,
                        None,
                        None,
                    )
                    .await
                    {
                        error!(
                            "Failed to mark invoice as expired {}: {}",
                            pending_reorg.payment_hash, e
                        );
                    }
                }
            }
            Err(e) => {
                warn!(
                    "Failed to lookup invoice {}: {}",
                    pending_reorg.payment_hash, e
                );
            }
        }
    }

    // If multiple settled reorgs, only execute the one with most blocks
    // and remove all others (respects cooldown limit)
    if !settled_reorgs.is_empty() {
        // Sort by blocks descending
        settled_reorgs.sort_by_key(|reorg| std::cmp::Reverse(reorg.blocks));

        let reorg_to_execute = &settled_reorgs[0];

        // Execute the biggest one
        match execute_reorg_internal(state, reorg_to_execute).await {
            Err(e) => {
                // Don't remove from pending so we can retry later
                error!(
                    "Failed to execute reorg for {}: {}",
                    reorg_to_execute.payment_hash, e
                );
            }
            Ok(()) => {
                // Successfully executed, now mark all other settled reorgs as skipped
                for (i, pending_reorg) in settled_reorgs.iter().enumerate() {
                    if i > 0 {
                        warn!(
                        "Marking settled but unexecuted reorg as skipped (cooldown limit): {} blocks for user {} (payment_hash: {})",
                        pending_reorg.blocks, pending_reorg.username, pending_reorg.payment_hash
                    );
                        if let Err(e) = update_reorg_status(
                            pool,
                            &pending_reorg.payment_hash,
                            "skipped",
                            None,
                            None,
                            None,
                        )
                        .await
                        {
                            error!(
                                "Failed to update reorg status {}: {}",
                                pending_reorg.payment_hash, e
                            );
                        }
                    }
                }
            }
        }
    }

    // Subscribe to invoice updates
    info!("Subscribing to mainnet LND invoice updates...");
    let subscribe_request = lnrpc::InvoiceSubscription {
        add_index: 0,
        settle_index: 0,
    };

    let mut stream = mainnet_client
        .clone()
        .subscribe_invoices(subscribe_request)
        .await?
        .into_inner();

    // Process invoice updates
    while let Some(invoice) = stream.message().await? {
        let payment_hash = hex::encode(&invoice.r_hash);

        // Process settled invoices
        if invoice.state == lnrpc::invoice::InvoiceState::Settled as i32 {
            // Check if this is a pending reorg
            if let Ok(Some(pending_reorg)) = get_pending_reorg(pool, &payment_hash).await {
                info!(
                    "Invoice settled for reorg: {} blocks for user {}",
                    pending_reorg.blocks, pending_reorg.username
                );

                // Execute the reorg
                if let Err(e) = execute_reorg_internal(state, &pending_reorg).await {
                    error!("Failed to execute reorg: {}", e);
                    // Keep in pending so we can retry later
                } else {
                    info!("Successfully executed reorg for payment {}", payment_hash);
                }
            }
        }
        // Process canceled/expired invoices
        else if invoice.state == lnrpc::invoice::InvoiceState::Canceled as i32 {
            // Check if this is a pending reorg
            if let Ok(Some(pending_reorg)) = get_pending_reorg(pool, &payment_hash).await {
                info!(
                    "Invoice expired for reorg: {} blocks for user {} (payment_hash: {})",
                    pending_reorg.blocks, pending_reorg.username, payment_hash
                );

                if let Err(e) =
                    update_reorg_status(pool, &payment_hash, "expired", None, None, None).await
                {
                    error!("Failed to mark invoice as expired {}: {}", payment_hash, e);
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::reorg_target_height;

    #[test]
    fn targets_exact_number_of_tip_blocks() {
        assert_eq!(reorg_target_height(100, 1), Some(100));
        assert_eq!(reorg_target_height(100, 5), Some(96));
        assert_eq!(reorg_target_height(4, 5), None);
        assert_eq!(reorg_target_height(100, 0), None);
    }
}

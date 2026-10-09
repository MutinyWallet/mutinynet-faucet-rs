use anyhow::{anyhow, Result};
use bitcoin::hashes::{sha256, Hash};
use bitcoincore_rpc::RpcApi;
use ldk_server_client::client::LdkServerClient;
use ldk_server_client::ldk_server_grpc::api::{
    Bolt11ClaimForIdRequest, Bolt11FailForIdRequest, Bolt11ReceiveForHashRequest,
    GetPaymentDetailsRequest,
};
use ldk_server_client::ldk_server_grpc::events::{event_envelope::Event, PaymentClaimable};
use ldk_server_client::ldk_server_grpc::types::{
    bolt11_invoice_description, Bolt11InvoiceDescription, PaymentStatus,
};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{interval, sleep, Duration};

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

/// How long a reorg invoice stays payable. LDK accepts payments for a while
/// past the invoice's stated expiry, so reorg invoices are hold invoices and
/// payments arriving after this window are failed back instead of claimed.
const REORG_INVOICE_EXPIRY_SECS: u32 = 600; // 10 minutes

/// How often pending reorgs are checked against ldk-server, to expire unpaid
/// invoices and to catch payments whose events were missed.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug)]
struct PendingReorg {
    payment_hash: String,
    blocks: u8,
    username: String,
    created_at: i64,
    /// Hex preimage of the hold invoice. None for reorgs created on LND.
    preimage: Option<String>,
}

impl PendingReorg {
    fn is_expired(&self, now: i64) -> bool {
        now > self.created_at + REORG_INVOICE_EXPIRY_SECS as i64
    }
}

type ReorgRow = (String, i64, String, i64, Option<String>);

impl From<ReorgRow> for PendingReorg {
    fn from((payment_hash, blocks, username, created_at, preimage): ReorgRow) -> Self {
        PendingReorg {
            payment_hash,
            blocks: blocks as u8,
            username,
            created_at,
            preimage,
        }
    }
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

    // Databases created before the move to ldk-server lack this column.
    let has_preimage: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM pragma_table_info('reorgs') WHERE name = 'preimage'")
            .fetch_one(&pool)
            .await?;
    if has_preimage.0 == 0 {
        sqlx::query("ALTER TABLE reorgs ADD COLUMN preimage TEXT")
            .execute(&pool)
            .await?;
    }

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
    preimage: &str,
    blocks: u8,
    username: &str,
) -> Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;

    sqlx::query(
        "INSERT INTO reorgs (payment_hash, preimage, blocks, username, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(payment_hash)
    .bind(preimage)
    .bind(blocks as i64)
    .bind(username)
    .bind(now)
    .execute(pool)
    .await?;

    Ok(())
}

/// Get a reorg of any status by payment hash, along with its status
async fn get_reorg(
    pool: &SqlitePool,
    payment_hash: &str,
) -> Result<Option<(PendingReorg, String)>> {
    let result = sqlx::query_as::<_, (String, i64, String, i64, Option<String>, String)>(
        "SELECT payment_hash, blocks, username, created_at, preimage, status FROM reorgs WHERE payment_hash = ?",
    )
    .bind(payment_hash)
    .fetch_optional(pool)
    .await?;

    Ok(result.map(
        |(payment_hash, blocks, username, created_at, preimage, status)| {
            let row = (payment_hash, blocks, username, created_at, preimage);
            (row.into(), status)
        },
    ))
}

/// Get all pending reorgs
async fn get_all_reorgs(pool: &SqlitePool) -> Result<Vec<PendingReorg>> {
    let results = sqlx::query_as::<_, ReorgRow>(
        "SELECT payment_hash, blocks, username, created_at, preimage FROM reorgs WHERE status = 'pending'",
    )
    .fetch_all(pool)
    .await?;

    Ok(results.into_iter().map(PendingReorg::from).collect())
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

    // Keep the availability check, invoice creation, and pending-row
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

    // Generate a hold invoice on the mainnet node
    let mainnet_client = state
        .mainnet_ldk_client
        .as_ref()
        .ok_or_else(|| anyhow!("Mainnet node not configured"))?;

    let blocks_word = if request.blocks == 1 {
        "block"
    } else {
        "blocks"
    };
    let memo = format!(
        "Mutinynet Reorg: {} {} for user {}",
        request.blocks, blocks_word, user.username
    );

    let preimage: [u8; 32] = rand::random();
    let payment_hash = sha256::Hash::hash(&preimage).to_string();

    let response = mainnet_client
        .bolt11_receive_for_hash(Bolt11ReceiveForHashRequest {
            amount_msat: Some(*amount_sats * 1_000),
            description: Some(Bolt11InvoiceDescription {
                kind: Some(bolt11_invoice_description::Kind::Direct(memo)),
            }),
            expiry_secs: REORG_INVOICE_EXPIRY_SECS,
            payment_hash: payment_hash.clone(),
        })
        .await?;

    // Store in database
    store_pending_reorg(
        pool,
        &payment_hash,
        &hex::encode(preimage),
        request.blocks,
        &user.username,
    )
    .await?;

    info!(
        "Generated reorg invoice for user {}: {} blocks, payment_hash: {}",
        user.username, request.blocks, payment_hash
    );

    Ok(ReorgInvoiceResponse {
        invoice: response.invoice,
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

/// Background task that watches mainnet node payments and executes reorgs
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
        .mainnet_ldk_client
        .as_ref()
        .ok_or_else(|| anyhow!("Mainnet node not configured"))?;

    let pool = state
        .reorg_db
        .as_ref()
        .ok_or_else(|| anyhow!("Reorg database not initialized"))?;

    // Subscribe before the first reconcile so no payment slips between them.
    info!("Subscribing to mainnet ldk-server events...");
    let mut events = mainnet_client.subscribe_events().await?;

    // The first tick fires immediately, so pending reorgs are reconciled on
    // every (re)connect.
    let mut reconcile = interval(RECONCILE_INTERVAL);
    loop {
        tokio::select! {
            _ = reconcile.tick() => {
                if let Err(e) = reconcile_pending_reorgs(state, mainnet_client, pool).await {
                    error!("Failed to reconcile pending reorgs: {}", e);
                }
            }
            event = events.next_message() => {
                let event = event.ok_or_else(|| anyhow!("ldk-server event stream ended"))??;
                if let Some(Event::PaymentClaimable(claimable)) = event.event {
                    handle_claimable(state, mainnet_client, pool, claimable).await;
                }
            }
        }
    }
}

/// Claim the payment for `reorg` if its invoice is still payable, otherwise
/// fail it back so the buyer is refunded. Returns whether it was claimed.
async fn claim_or_fail(
    state: &AppState,
    client: &LdkServerClient,
    reorg: &PendingReorg,
    is_pending: bool,
    amount_msat: u64,
) -> Result<bool> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let price_msat = state
        .reorg_config
        .pricing
        .get(&reorg.blocks)
        .map(|sats| sats * 1_000);

    let payable = is_pending
        && !reorg.is_expired(now)
        && price_msat.is_some_and(|price| amount_msat >= price);

    // Bolt11 payment ids are the payment hash.
    match reorg.preimage.as_ref().filter(|_| payable) {
        Some(preimage) => {
            client
                .bolt11_claim_for_id(Bolt11ClaimForIdRequest {
                    payment_id: reorg.payment_hash.clone(),
                    claimable_amount_msat: Some(amount_msat),
                    preimage: preimage.clone(),
                })
                .await?;
            Ok(true)
        }
        None => {
            client
                .bolt11_fail_for_id(Bolt11FailForIdRequest {
                    payment_id: reorg.payment_hash.clone(),
                })
                .await?;
            Ok(false)
        }
    }
}

/// Handle a payment arriving for a hold invoice. Payments for invoices that
/// aren't ours are left alone, since other services may share the node.
async fn handle_claimable(
    state: &AppState,
    client: &LdkServerClient,
    pool: &SqlitePool,
    claimable: PaymentClaimable,
) {
    let (reorg, status) = match get_reorg(pool, &claimable.payment_id).await {
        Ok(Some(found)) => found,
        Ok(None) => return,
        Err(e) => {
            // Left unhandled, LDK fails the payment back at its claim deadline.
            error!(
                "Failed to look up reorg for payment {}: {}",
                claimable.payment_id, e
            );
            return;
        }
    };

    match claim_or_fail(
        state,
        client,
        &reorg,
        status == "pending",
        claimable.claimable_amount_msat,
    )
    .await
    {
        Ok(true) => {
            info!(
                "Invoice paid for reorg: {} blocks for user {}",
                reorg.blocks, reorg.username
            );
            if let Err(e) = execute_reorg_internal(state, &reorg).await {
                // Still pending, so the next reconcile retries it.
                error!("Failed to execute reorg: {}", e);
            } else {
                info!(
                    "Successfully executed reorg for payment {}",
                    reorg.payment_hash
                );
            }
        }
        Ok(false) => warn!(
            "Failed back payment for {} reorg (payment_hash: {})",
            status, reorg.payment_hash
        ),
        Err(e) => error!(
            "Failed to claim or fail payment {}: {}",
            reorg.payment_hash, e
        ),
    }
}

async fn mark_expired(pool: &SqlitePool, reorg: &PendingReorg) {
    info!(
        "Invoice expired for reorg: {} blocks for user {} (payment_hash: {})",
        reorg.blocks, reorg.username, reorg.payment_hash
    );
    if let Err(e) =
        update_reorg_status(pool, &reorg.payment_hash, "expired", None, None, None).await
    {
        error!(
            "Failed to mark invoice as expired {}: {}",
            reorg.payment_hash, e
        );
    }
}

/// Bring pending reorgs in line with ldk-server: execute paid reorgs, claim
/// payments whose events were missed, and expire unpaid invoices.
async fn reconcile_pending_reorgs(
    state: &AppState,
    client: &LdkServerClient,
    pool: &SqlitePool,
) -> Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let mut settled_reorgs = Vec::new();

    for pending_reorg in get_all_reorgs(pool).await? {
        let payment = match client
            .get_payment_details(GetPaymentDetailsRequest {
                payment_id: pending_reorg.payment_hash.clone(),
            })
            .await
        {
            Ok(response) => response.payment,
            Err(e) => {
                warn!(
                    "Failed to lookup invoice {}: {}",
                    pending_reorg.payment_hash, e
                );
                continue;
            }
        };

        match payment.as_ref().map(|p| p.status()) {
            Some(PaymentStatus::Succeeded) => {
                info!(
                    "Found settled invoice for pending reorg: {} blocks for user {}",
                    pending_reorg.blocks, pending_reorg.username
                );
                settled_reorgs.push(pending_reorg);
            }
            // Claimable, but its PaymentClaimable event was missed.
            Some(PaymentStatus::Pending) => {
                let amount_msat = payment.and_then(|p| p.amount_msat).unwrap_or(0);
                match claim_or_fail(state, client, &pending_reorg, true, amount_msat).await {
                    Ok(true) => settled_reorgs.push(pending_reorg),
                    Ok(false) => {}
                    Err(e) => warn!(
                        "Failed to claim or fail payment {}: {}",
                        pending_reorg.payment_hash, e
                    ),
                }
            }
            _ if pending_reorg.is_expired(now) => mark_expired(pool, &pending_reorg).await,
            _ => {}
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

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{init_reorg_db, reorg_target_height, PendingReorg, REORG_INVOICE_EXPIRY_SECS};

    #[test]
    fn targets_exact_number_of_tip_blocks() {
        assert_eq!(reorg_target_height(100, 1), Some(100));
        assert_eq!(reorg_target_height(100, 5), Some(96));
        assert_eq!(reorg_target_height(4, 5), None);
        assert_eq!(reorg_target_height(100, 0), None);
    }

    #[test]
    fn invoice_expires_after_window() {
        let reorg = PendingReorg {
            payment_hash: String::new(),
            blocks: 1,
            username: String::new(),
            created_at: 1_000,
            preimage: None,
        };
        let deadline = 1_000 + REORG_INVOICE_EXPIRY_SECS as i64;
        assert!(!reorg.is_expired(deadline));
        assert!(reorg.is_expired(deadline + 1));
    }

    #[tokio::test]
    async fn adds_preimage_column_to_existing_db() {
        let path = std::env::temp_dir().join(format!("reorg-migrate-{}.db", rand::random::<u64>()));
        let db_path = path.to_str().unwrap();
        std::fs::File::create(&path).unwrap();

        // Schema from before the move to ldk-server.
        let pool = sqlx::SqlitePool::connect(&format!("sqlite:{db_path}"))
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE reorgs (payment_hash TEXT PRIMARY KEY, blocks INTEGER NOT NULL, \
             username TEXT NOT NULL, created_at INTEGER NOT NULL, \
             status TEXT NOT NULL DEFAULT 'pending', executed_at INTEGER, \
             invalidated_block_height INTEGER, invalidated_block_hash TEXT)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        // Running twice must not try to add the column again.
        init_reorg_db(db_path).await.unwrap().close().await;
        let pool = init_reorg_db(db_path).await.unwrap();
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM pragma_table_info('reorgs') WHERE name = 'preimage'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1);

        pool.close().await;
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{db_path}{suffix}"));
        }
    }
}

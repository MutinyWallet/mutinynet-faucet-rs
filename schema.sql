-- Reorg invoice history with status tracking (for accounting)
CREATE TABLE IF NOT EXISTS reorgs (
    payment_hash TEXT PRIMARY KEY,
    blocks INTEGER NOT NULL,
    username TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',  -- 'pending', 'executed', 'skipped', 'expired'
    executed_at INTEGER,  -- timestamp when executed (NULL if not executed)
    invalidated_block_height INTEGER,  -- block height that was invalidated
    invalidated_block_hash TEXT  -- block hash that was invalidated
);

-- Cooldown tracking (stores timestamp of last executed reorg)
CREATE TABLE IF NOT EXISTS reorg_cooldown (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    last_reorg_timestamp INTEGER NOT NULL
);

-- Initialize cooldown table with zero timestamp
INSERT OR IGNORE INTO reorg_cooldown (id, last_reorg_timestamp) VALUES (1, 0);

# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

MutinyNet Faucet is a Rust-based REST API faucet service for Bitcoin testnet/signet/regtest networks. It dispenses test
bitcoin via on-chain transactions, Lightning Network payments (bolt11 invoices), LNURL, and Nostr zaps. The service
includes GitHub OAuth authentication with JWT tokens and rate-limiting based on IP, Bitcoin address, and GitHub user
identity.

## Development Commands

**Build and run:**

```bash
cargo build
cargo run
```

**Running tests:**

```bash
cargo test
```

**Environment setup:**
Copy `.env.sample` to `.env.local` and configure with LND and Bitcoin Core connection details.

## Architecture

### Core Application State (`AppState`)

The application is built around a shared state struct containing:

- `lightning_client`: LND gRPC client for Lightning and on-chain operations
- `keys`: Nostr keypair for signing zap requests
- `network`: Bitcoin network (regtest/testnet/signet)
- `lnurl`: Async LNURL client for handling LNURL-pay and lightning addresses
- `payments`: Rate-limiting tracker using `PaymentsByIp`
- `auth`: GitHub OAuth and JWT authentication state

### Authentication System (`auth.rs`)

- **GitHub OAuth Flow**: Web-based (`/auth/github` + `/auth/github/callback`) and device-based (`/auth/github/device`)
  authentication
- **JWT Tokens**: 24-hour expiry for web login, 31-day expiry for device login
- **Auth Middleware**: `auth_middleware` extracts and validates JWT tokens from Authorization Bearer headers, injects
  `AuthUser` into request extensions
- **User Management**: SQLite tables in the users database (see "User Lists" below):
    - `banned_domains`: Email domain blacklist
    - `banned_users`: Individual user email blacklist
    - `whitelisted_users`: Users exempt from bans
    - `premium_users`: Users exempt from rate limits and from bans

### Rate Limiting (`payments.rs`)

The `PaymentsByIp` struct tracks payments over 24-hour rolling windows:

- Tracks by IP address (`x-forwarded-for` header)
- Tracks by Bitcoin address (for on-chain requests)
- Tracks by GitHub user (format: `github:{email}`)
- Max daily limit: 1,000,000 sats per identifier
- Premium users bypass rate limits

### Payment Endpoints

**On-chain** (`/api/onchain` - requires auth):

- Uses LND's `SendCoins` RPC
- Validates addresses against configured network
- Supports BIP21 URIs with amount parameter

**Lightning** (`/api/lightning` - requires auth):

- Supports bolt11 invoices, LNURL-pay, lightning addresses, and Nostr zap requests
- LNURL resolution and invoice generation via `lnurl-rs`
- Nostr: Fetches user metadata from relays, constructs zap events signed with faucet's nsec

**LNURL Withdrawal** (`/api/lnurlw` + `/api/lnurlw/callback` - no auth):

- Provides LNURL-withdraw endpoint for wallet scanning
- Rate-limited by IP only (no user tracking)

**Bolt11 Invoice Generation** (`/api/bolt11` - no auth):

- Creates invoices via LND's `AddInvoice` RPC
- Used for testing receive flows

**Channel Opening** (`/api/channel` - requires auth):

- Opens Lightning channels via LND's `OpenChannel` RPC
- Requires peer pubkey, host, capacity, and push amount

### Nostr Integration (`nostr_dms.rs`)

Background task listening for Nostr DMs:

- Connects to hardcoded relay list (RELAYS constant)
- Processes encrypted DMs containing payment requests
- Responds with payment confirmations via DM
- Auto-reconnects on errors

### Entry Point (`main.rs`)

- **Setup**: `setup()` loads env vars, connects to LND, initializes auth state
- **Middleware**: CORS (allow all origins), JWT auth on protected routes
- **Graceful Shutdown**: Handles SIGTERM/SIGINT signals
- **Server**: Binds to `0.0.0.0:3001`

## Important Constants

- `MAX_SEND_AMOUNT`: 1,000,000 sats per payment
- `CACHE_DURATION`: 86,400 seconds (24 hours) for rate limit tracking

## LND Connection

The application uses `tonic_openssl_lnd` for LND gRPC communication. Required environment variables:

- `GRPC_HOST`, `GRPC_PORT`: LND gRPC endpoint
- `TLS_CERT_PATH`: Path to LND's tls.cert
- `ADMIN_MACAROON_PATH`: Path to admin.macaroon (requires send permissions)

The same LND client is used for both Lightning operations (invoices, payments) and on-chain wallet operations (send
coins, channel opening).

## User Lists

Ban, premium, and whitelist lists are stored in SQLite (`USERS_DB_PATH`, default `users.db`) and cached in memory
by `UsersCache` (`auth.rs`). The auth middleware checks the cache on every request, so changes apply without a
restart as long as they go through the admin API.

- Values are trimmed and lowercased on write and on load; matching is case-insensitive
- A user is banned if their email is in `banned_users` or their domain is in `banned_domains`, unless they are in
  `whitelisted_users` or `premium_users`
- Editing the database directly does not update the cache; the change is only seen after a restart
- `faucet_config/*.txt` files are a legacy format, imported once at startup only into empty tables

### Admin API (`admin.rs`)

`/api/admin/:list` where `:list` is `banned_domains`, `banned_users`, `whitelisted_users`, or `premium_users`.
Requires `Authorization: Bearer $ADMIN_TOKEN`; returns 404 for all requests if `ADMIN_TOKEN` is unset.

- `GET`: List entries (`{"entries": [...]}`)
- `POST {"value": "..."}`: Add an entry (idempotent, 201)
- `DELETE {"value": "..."}`: Remove an entry (200, or 404 if absent)

Writes update both the database and the in-memory cache.

## Paid Block Reorg Feature

Premium feature allowing authenticated users to trigger block reorganizations by paying with real Lightning payments.

**Architecture:**

- `src/reorg.rs`: Invoice generation and reorg execution
- Two-step flow: generate mainnet invoice → pay → execute reorg
- Mainnet ldk-server client (`ldk-server-client`) for accepting real payments (separate from the testnet/regtest
  faucet LND)
- Bitcoin Core RPC integration for `invalidate_block` calls
- Works on both regtest and signet networks
- Progressive pricing: 10k, 20k, 35k, 50k, 75k sats for 1-5 blocks
- 1-hour cooldown between reorgs

**Endpoints:**

- `POST /api/reorg/invoice`: Generate mainnet invoice and store pending reorg (requires auth)

**Environment Variables:**

- `MAINNET_LDK_SERVER_URL/TLS_CERT_PATH/MACAROON_PATH`: Mainnet ldk-server connection (also used by L402)
- `REORG_ENABLED`: Feature flag (must be "true")
- `REORG_COOLDOWN_SECONDS`: Cooldown duration (default: 3600)
- `REORG_DB_PATH`: SQLite database path (default: "reorg.db")
- Existing Bitcoin Core RPC vars: `BITCOIN_RPC_HOST_AND_PORT`, `BITCOIN_RPC_USER`, `BITCOIN_RPC_PASSWORD`

**How it works:**

- User generates invoice via `/api/reorg/invoice`, stored in SQLite database
- Invoices are hold invoices (`Bolt11ReceiveForHash`); the preimage is stored with the reorg
- Background task subscribes to ldk-server events and claims `PaymentClaimable` payments only while the reorg is
  pending and inside the 10-minute invoice window; late payments are failed back (LDK accepts payments past the
  stated invoice expiry, so this is enforced here)
- Every 30 seconds pending reorgs are reconciled: paid ones execute, missed claimables are claimed, unpaid ones expire
- When invoice is paid, automatically executes reorg
- Calls `invalidate_block` on Bitcoin Core to invalidate N blocks from the chain tip
- On regtest: Chain height decreases; new blocks can be mined to rebuild
- On signet: Chain height decreases; waits for new blocks from the network
- Cooldown stored in database, persists across restarts
- On startup, checks all pending reorgs and executes if already paid

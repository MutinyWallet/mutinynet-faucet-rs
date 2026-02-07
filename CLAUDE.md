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
- **User Management**: Configuration files in `faucet_config/` directory:
    - `banned_domains.txt`: Email domain blacklist
    - `banned_users.txt`: Individual user email blacklist
    - `whitelisted_users.txt`: Users exempt from bans
    - `premium_users.txt`: Users exempt from rate limits

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

## Configuration Files

All user management files are read fresh on each request (no caching), located in `faucet_config/`:

- Text files with one entry per line
- Empty lines and whitespace are ignored
- Email matching is case-insensitive for banned domains

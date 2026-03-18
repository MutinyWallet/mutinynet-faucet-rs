# Analytics API

The faucet records every payment to a local SQLite database and exposes read-only endpoints for building dashboards. All endpoints return JSON.

## Configuration

| Environment Variable | Default | Description |
|---|---|---|
| `ANALYTICS_DB_PATH` | `analytics.db` | Path to the SQLite database file |
| `ANALYTICS_TOKEN` | _(none)_ | API token for analytics endpoints. **Required** — endpoints return 404 if unset. |

The database is created automatically on startup. No migration steps are needed.

## Authentication

All analytics endpoints require a Bearer token matching the `ANALYTICS_TOKEN` environment variable:

```
Authorization: Bearer <your-analytics-token>
```

Returns `401 Unauthorized` if the token is missing or wrong. Returns `404 Not Found` if `ANALYTICS_TOKEN` is not configured (endpoints are hidden).

## Payment Types

Every recorded payment has a `payment_type` field. Possible values:

| Type | Source | Description |
|---|---|---|
| `onchain` | `POST /api/onchain` | On-chain bitcoin send |
| `lightning` | `POST /api/lightning`, `GET /api/lnurlw/callback` | Lightning invoice payment (includes LNURL-pay, lightning addresses, and zaps) |
| `channel` | `POST /api/channel` | Lightning channel open |
| `bolt11` | `POST /api/bolt11` | Invoice generation (receive-side testing) |
| `nostr_dm` | Nostr DM listener | Lightning payment triggered via Nostr DM |
| `nostr_dm_onchain` | Nostr DM listener | On-chain payment triggered via Nostr DM |
| `l402_issued` | `POST /api/l402`, `GET /api/l402` | L402 authentication token issued (mainnet invoice created) |
| `l402_paid` | `GET /api/l402/check` | L402 invoice confirmed paid (deduplicated by payment hash) |

## Common Query Parameters

All endpoints (except `/recent`) accept:

| Param | Type | Default | Description |
|---|---|---|---|
| `hours` | integer | `24` | Rolling window to look back |
| `payment_type` | string | _(all)_ | Filter to a single payment type |

## Endpoints

### `GET /api/analytics/summary`

High-level KPIs for the time window. Use for dashboard header cards.

**Extra params:** none

**Response:**

```json
{
  "hours": 24,
  "total_count": 142,
  "total_sats": 84200000,
  "unique_users": 37,
  "avg_sats": 593000,
  "by_type": [
    { "payment_type": "onchain", "count": 80, "total_sats": 60000000 },
    { "payment_type": "lightning", "count": 50, "total_sats": 20000000 },
    { "payment_type": "channel", "count": 12, "total_sats": 4200000 }
  ]
}
```

---

### `GET /api/analytics/timeseries`

Bucketed time series with per-type breakdown in each bucket. Use for stacked area/bar charts.

**Extra params:**

| Param | Type | Default | Description |
|---|---|---|---|
| `interval` | string | `hour` | Bucket size: `hour` or `day` |

**Response:**

```json
{
  "hours": 48,
  "interval": "hour",
  "buckets": [
    {
      "time": "2026-03-16T14:00:00Z",
      "count": 5,
      "total_sats": 2500000,
      "by_type": [
        { "payment_type": "onchain", "count": 3, "total_sats": 2000000 },
        { "payment_type": "lightning", "count": 2, "total_sats": 500000 }
      ]
    },
    {
      "time": "2026-03-16T15:00:00Z",
      "count": 8,
      "total_sats": 4100000,
      "by_type": [
        { "payment_type": "onchain", "count": 4, "total_sats": 3000000 },
        { "payment_type": "lightning", "count": 3, "total_sats": 900000 },
        { "payment_type": "channel", "count": 1, "total_sats": 200000 }
      ]
    }
  ]
}
```

---

### `GET /api/analytics/users`

Top users ranked by total sats, with per-type breakdown for each user.

**Extra params:**

| Param | Type | Default | Description |
|---|---|---|---|
| `limit` | integer | `50` | Max number of users to return |

**Response:**

```json
{
  "hours": 24,
  "users": [
    {
      "user": "alice@example.com",
      "count": 12,
      "total_sats": 8400000,
      "last_payment": 1710700000,
      "by_type": [
        { "payment_type": "onchain", "count": 8, "total_sats": 6000000 },
        { "payment_type": "lightning", "count": 4, "total_sats": 2400000 }
      ]
    },
    {
      "user": "192.168.1.50",
      "count": 3,
      "total_sats": 1500000,
      "last_payment": 1710695000,
      "by_type": [
        { "payment_type": "lightning", "count": 3, "total_sats": 1500000 }
      ]
    }
  ]
}
```

The `user` field is the GitHub email when authenticated, or the IP address for unauthenticated requests (LNURL-withdraw, Nostr DMs). `last_payment` is a Unix timestamp.

---

### `GET /api/analytics/recent`

Most recent individual payments. Use for a live activity feed.

**Params:**

| Param | Type | Default | Description |
|---|---|---|---|
| `limit` | integer | `50` | Max number of payments to return |
| `payment_type` | string | _(all)_ | Filter to a single payment type |

Note: this endpoint does **not** accept `hours` — it always returns the N most recent payments regardless of age.

**Response:**

```json
{
  "payments": [
    {
      "id": 1042,
      "created_at": 1710700123,
      "payment_type": "onchain",
      "amount_sats": 500000,
      "user": "alice@example.com",
      "destination": "tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx"
    },
    {
      "id": 1041,
      "created_at": 1710700100,
      "payment_type": "lightning",
      "amount_sats": 100000,
      "user": "192.168.1.50",
      "destination": "lnbc1u1pj..."
    }
  ]
}
```

### `GET /api/analytics/domains`

Usage breakdown by email domain (gmail.com, hotmail.com, proton.me, etc.). Only includes authenticated users with email usernames — excludes L402 users, IPs, and Nostr pubkeys.

**Extra params:**

| Param | Type | Default | Description |
|---|---|---|---|
| `limit` | integer | `50` | Max number of domains to return |

**Response:**

```json
{
  "hours": 24,
  "total_count": 120,
  "total_sats": 72000000,
  "domains": [
    { "domain": "gmail.com", "count": 80, "total_sats": 50000000, "unique_users": 25 },
    { "domain": "proton.me", "count": 20, "total_sats": 12000000, "unique_users": 8 },
    { "domain": "hotmail.com", "count": 12, "total_sats": 6000000, "unique_users": 3 },
    { "domain": "example.com", "count": 8, "total_sats": 4000000, "unique_users": 1 }
  ]
}
```

---

### `GET /api/analytics/l402`

L402 authentication stats. Shows tokens issued (mainnet invoices generated) and payments made by L402-authenticated users, each with their own timeseries.

**Extra params:**

| Param | Type | Default | Description |
|---|---|---|---|
| `interval` | string | `hour` | Bucket size: `hour` or `day` |

Note: this endpoint does **not** accept `payment_type` — it's L402-specific.

**Response:**

```json
{
  "hours": 24,
  "interval": "hour",
  "issued": {
    "count": 45,
    "total_sats": 45000,
    "timeseries": [
      { "time": "2026-03-16T14:00:00Z", "count": 3, "total_sats": 3000 },
      { "time": "2026-03-16T15:00:00Z", "count": 5, "total_sats": 5000 }
    ]
  },
  "paid": {
    "count": 30,
    "total_sats": 30000,
    "timeseries": [
      { "time": "2026-03-16T14:00:00Z", "count": 2, "total_sats": 2000 },
      { "time": "2026-03-16T15:00:00Z", "count": 4, "total_sats": 4000 }
    ]
  },
  "usage": {
    "count": 12,
    "total_sats": 6000000,
    "unique_tokens": 8,
    "timeseries": [
      { "time": "2026-03-16T14:00:00Z", "count": 1, "total_sats": 500000 },
      { "time": "2026-03-16T15:00:00Z", "count": 3, "total_sats": 1500000 }
    ]
  }
}
```

- `issued` — L402 tokens created (mainnet invoices generated).
- `paid` — L402 invoices actually paid (confirmed settled). Deduplicated by payment hash. Compare `issued` vs `paid` to see conversion rate. `total_sats` is revenue collected.
- `usage` — faucet payments made by users who authenticated via L402. `unique_tokens` is the number of distinct L402 tokens used.

---

## Database Schema

```sql
CREATE TABLE faucet_payments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    payment_type TEXT NOT NULL,
    amount_sats INTEGER NOT NULL,
    username TEXT,
    ip_address TEXT NOT NULL,
    destination TEXT
);
```

Indexes on `created_at`, `username`, and `payment_type`.

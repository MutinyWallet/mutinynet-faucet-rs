# Admin API

Manage user lists (bans, whitelists, premium users) stored in a local SQLite database. All endpoints return JSON.

## Configuration

| Environment Variable | Default | Description |
|---|---|---|
| `USERS_DB_PATH` | `users.db` | Path to the SQLite database file |
| `ADMIN_TOKEN` | _(none)_ | API token for admin endpoints. **Required** — endpoints return 404 if unset. |

The database is created automatically on startup. On first run, if `faucet_config/*.txt` files exist, their contents are migrated into the database automatically.

## Authentication

All admin endpoints require a Bearer token matching the `ADMIN_TOKEN` environment variable:

```
Authorization: Bearer <your-admin-token>
```

Returns `401 Unauthorized` if the token is missing or wrong. Returns `404 Not Found` if `ADMIN_TOKEN` is not configured (endpoints are hidden).

## User Lists

| List Name | Column | Description |
|---|---|---|
| `banned_domains` | `domain` | Email domains whose users are blocked at login (case-insensitive match) |
| `banned_users` | `email` | Individual emails blocked at login |
| `whitelisted_users` | `email` | Emails exempt from all bans (overrides both domain and user bans) |
| `premium_users` | `email` | Emails exempt from bans **and** rate limits |

### Ban check order

When a user authenticates, bans are evaluated in this order:

1. If email is in `whitelisted_users` — **not banned**
2. If email is in `premium_users` — **not banned**
3. If email domain is in `banned_domains` — **banned**
4. If email is in `banned_users` — **banned**
5. Otherwise — **not banned**

Banned users are rejected at GitHub OAuth login and re-checked on every authenticated request.

## Endpoints

All endpoints use the path `/api/admin/:list` where `:list` is one of: `banned_domains`, `banned_users`, `whitelisted_users`, `premium_users`.

Any other `:list` value returns `404 Not Found`.

---

### `GET /api/admin/:list`

List all entries in a user list, sorted alphabetically.

**Response:**

```json
{
  "entries": [
    "gmail.com",
    "yahoo.com"
  ]
}
```

Returns an empty array if the list has no entries.

---

### `POST /api/admin/:list`

Add an entry to a user list. Duplicates are silently ignored.

**Request body:**

```json
{
  "value": "spam.com"
}
```

**Response:** `201 Created`

Returns `400 Bad Request` if `value` is empty or whitespace-only.

---

### `DELETE /api/admin/:list`

Remove an entry from a user list.

**Request body:**

```json
{
  "value": "spam.com"
}
```

**Response:** `200 OK`

Returns `404 Not Found` if the entry doesn't exist.

---

## Examples

```bash
# List all banned domains
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  https://faucet.mutinynet.com/api/admin/banned_domains

# Ban a domain
curl -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"value": "spam.com"}' \
  https://faucet.mutinynet.com/api/admin/banned_domains

# Unban a domain
curl -X DELETE -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"value": "spam.com"}' \
  https://faucet.mutinynet.com/api/admin/banned_domains

# Add a premium user
curl -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"value": "vip@example.com"}' \
  https://faucet.mutinynet.com/api/admin/premium_users

# Whitelist a user
curl -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"value": "trusted@gmail.com"}' \
  https://faucet.mutinynet.com/api/admin/whitelisted_users
```

## Database Schema

```sql
CREATE TABLE banned_domains (
    domain TEXT PRIMARY KEY NOT NULL
);

CREATE TABLE banned_users (
    email TEXT PRIMARY KEY NOT NULL
);

CREATE TABLE whitelisted_users (
    email TEXT PRIMARY KEY NOT NULL
);

CREATE TABLE premium_users (
    email TEXT PRIMARY KEY NOT NULL
);
```

You can also manage entries directly via the SQLite CLI:

```bash
sqlite3 users.db "SELECT * FROM banned_domains"
sqlite3 users.db "INSERT INTO premium_users (email) VALUES ('user@example.com')"
```

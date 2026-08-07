# LND 0.21 upgrade checklist

The faucet uses `routerrpc.SendPaymentV2`, which is required because LND 0.21
removes `lnrpc.SendPaymentSync`.

## Before upgrading

1. Target the latest stable 0.21 patch release rather than 0.21.0.
2. Stop LND and back up its complete data directory, including channel backups,
   macaroons, TLS material, and all configured database files.
3. If `db.use-native-sql=true`, plan for the payment-store migration during
   startup. Set `db.skip-native-sql-migration=true` only if intentionally
   postponing that migration.
4. Remove `tor.v2` and any configured v2 `.onion` addresses. LND 0.21 rejects
   them at startup and at connection boundaries.
5. Confirm the faucet macaroon grants `offchain:write` for
   `routerrpc.SendPaymentV2`. The configured admin macaroon grants this.

## Rollback warning

For SQLite or PostgreSQL LND backends, treat the upgrade as one-way after any
channel is closed under 0.21. Older LND releases can interpret tombstoned
channels as open. This warning does not apply to bbolt or etcd backends.

## Smoke tests

After LND is synced and the faucet has restarted:

1. Call `GET /auth/check` and confirm the service is healthy.
2. Pay a routed BOLT11 invoice through `POST /api/lightning`.
3. Pay a self-payment invoice and confirm `allow_self_payment` still works.
4. Exercise a Nostr DM invoice payment if that listener is enabled.
5. Call the analytics balance endpoint and confirm on-chain and channel
   balances are populated.
6. Exercise `POST /api/onchain`, `POST /api/bolt11`, and `POST /api/channel` in
   the intended test environment.

The payment calls use a 60-second attempt timeout and preserve LND's legacy
default routing fee limit: 100% through 1,000 sats and 5% above 1,000 sats.

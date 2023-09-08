# MutinyNet Faucet API

1. Copy `.env.sample` to `.env.local` and fill it out with bitcoind and lnd connection info
2. Run `cargo build && cargo start`

## Endpoint examples

```sh
curl -X POST \
  http://localhost:3001/api/onchain \
  -H 'Content-Type: application/json' \
  -d '{"sats":10000,"address":"bcrt1..."}'
```

```sh
curl -X POST \
  http://localhost:3001/api/lightning \
  -H 'Content-Type: application/json' \
  -d '{"bolt11": "..."}'
```

```sh
curl -X POST \
  http://localhost:3001/api/bolt11 \
  -H 'Content-Type: application/json' \
  -d '{"amount_sats": 1234}'
```

```sh
curl -X POST \
  http://localhost:3001/api/channel \
  -H 'Content-Type: application/json' \
  -d '{"capacity": 2468,"push_amount": 1234,"pubkey":"023...","host":"127.0.0.1:9735"}'
```

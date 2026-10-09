# MutinyNet Faucet API

1. Copy `.env.sample` to `.env.local` and fill it out with bitcoind and lnd connection info
2. Run `cargo build && cargo start`

When upgrading the connected daemon from LND 0.20 to 0.21, follow the
[LND 0.21 upgrade checklist](docs/lnd-0.21-upgrade.md).

## Mainnet ldk-server macaroon

Paid reorgs and L402 take real payments through a mainnet
[ldk-server](https://github.com/lightningdevkit/ldk-server). Give the faucet
its own macaroon with only the permissions it needs, rather than
`admin.macaroon`:

```sh
ldk-server-cli create-macaroon mutinynet-faucet \
  --permissions node:read invoices:create payments:read payments:claim events:read \
  | jq -r .token > mutinynet-faucet.macaroon
chmod 600 mutinynet-faucet.macaroon
```

Run this on the ldk-server host. The CLI uses the node's `admin.macaroon`
and `tls.crt` from the default storage directory. If they live elsewhere,
pass `--macaroon <admin hex token>` and `--tls-cert <path>`.

| Permission        | Used for                                         |
|-------------------|--------------------------------------------------|
| `node:read`       | Checking the node is on mainnet at startup       |
| `invoices:create` | Creating reorg and L402 invoices                 |
| `payments:read`   | Checking whether an invoice was paid             |
| `payments:claim`  | Claiming or failing back reorg hold invoices     |
| `events:read`     | Watching for incoming reorg payments             |

Then point the faucet at it:

```sh
export MAINNET_LDK_SERVER_URL="127.0.0.1:3536"
export MAINNET_LDK_SERVER_TLS_CERT_PATH="/path/to/.ldk-server/tls.crt"
export MAINNET_LDK_SERVER_MACAROON_PATH="/path/to/mutinynet-faucet.macaroon"
```

To revoke it later, find its ID with `ldk-server-cli list-macaroons` and run
`ldk-server-cli revoke-macaroon <id>`.

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

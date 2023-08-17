```
curl -X POST \
  http://localhost:3001/api/onchain \
  -H 'Content-Type: application/json' \
  -d '{"sats":10000,"address":"bcrt1..."}'
```

```
curl -X POST \
  http://localhost:3001/api/lightning \
  -H 'Content-Type: application/json' \
  -d '{"bolt11": "..."}'
```

```
curl -X POST \
  http://localhost:3001/api/bolt11 \
  -H 'Content-Type: application/json' \
  -d '{"amount_sats": 1234}'
```
```
curl -X POST \
  http://localhost:3000/api/onchain \
  -H 'Content-Type: application/json' \
  -d '{"sats":10000,"address":"bcrt1..."}'
```

```
curl -X POST \
  http://localhost:3000/api/lightning \
  -H 'Content-Type: application/json' \
  -d '{"bolt11": "..."}'
```
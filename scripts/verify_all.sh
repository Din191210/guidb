#!/usr/bin/env bash
set -euo pipefail

cd /home/server1/guidb
cargo build -p guidb-server --offline

TMP_DATA="$(mktemp -d /tmp/guidb_e2e_XXXXXX)"
trap 'rm -rf "$TMP_DATA"; [[ -n "${SRV_PID:-}" ]] && kill "$SRV_PID" 2>/dev/null || true' EXIT

GUIDB_DATA_DIR="$TMP_DATA" GUIDB_STATIC_DIR="/home/server1/guidb/static" GUIDB_ADDR="127.0.0.1:8787" \
  /home/server1/guidb/target/debug/guidb-server >/tmp/guidb_server.log 2>&1 &
SRV_PID=$!

for _ in $(seq 1 30); do
  if curl -fsS http://127.0.0.1:8787/api/bootstrap >/dev/null 2>&1; then
    break
  fi
  sleep 0.25
done

echo "1) GET / HTML & WASM static check"
curl -fsS http://127.0.0.1:8787/ | grep -q "GuiDB Studio"
curl -fsSI http://127.0.0.1:8787/static/guidb_wasm.wasm | grep -q "200 OK"
curl -fsSI http://127.0.0.1:8787/static/app.css | grep -q "200 OK"
curl -fsSI http://127.0.0.1:8787/static/app.js | grep -q "200 OK"

echo "2) Live SQLite query & staged commit persistence check"
curl -fsS -X POST http://127.0.0.1:8787/api/connections/conn-commerce-core/mutate \
  -H 'Content-Type: application/json' \
  -d '{"mutations":[{"op":"insert","item":"customers","values":{"company_name":{"kind":"text","value":"PT Verifikasi Nyata"},"contact_email":{"kind":"text","value":"verify@nyata.id"},"city":{"kind":"text","value":"Surabaya"},"tier":{"kind":"text","value":"ENTERPRISE"},"credit_limit":{"kind":"int","value":75000000},"created_at":{"kind":"text","value":"2026-11-18T12:00:00Z"}}}]}' | grep -q '"committed_count":1'

sqlite3 "$TMP_DATA/commerce_core.sqlite" "SELECT company_name, city FROM customers WHERE contact_email='verify@nyata.id';" | grep -q "PT Verifikasi Nyata|Surabaya"

echo "3) Live MongoDB Aggregation Pipeline & Redis KV persistence check"
curl -fsS -X POST http://127.0.0.1:8787/api/connections/conn-telemetry-docs/query \
  -H 'Content-Type: application/json' \
  -d '{"item":"service_events","query":"[{\"$match\":{\"severity\":\"error\"}},{\"$group\":{\"_id\":\"$service\",\"events\":{\"$sum\":1}}}]"}' | grep -q "checkout-api"

curl -fsS -X POST http://127.0.0.1:8787/api/connections/conn-edge-kv/query \
  -H 'Content-Type: application/json' \
  -d '{"query":"SET e2e:token active EX 90"}' | grep -q "OK"
sqlite3 "$TMP_DATA/edge_kv.sqlite" "SELECT value_text, ttl_seconds FROM _guidb_kv WHERE key='e2e:token';" | grep -q "active|90"

echo "4) Live Schema Sync Diff check"
curl -fsS -X POST http://127.0.0.1:8787/api/schema-sync/diff \
  -H 'Content-Type: application/json' \
  -d '{"source_conn_id":"conn-commerce-core","target_conn_id":"conn-commerce-staging"}' | grep -q "shipments"

echo "ALL E2E CURL + SQLITE DB CHECKS PASSED"

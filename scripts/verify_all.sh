#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
cargo build -p guidb-server --offline

TMP_DATA="$(mktemp -d /tmp/guidb_e2e_XXXXXX)"
trap 'kill "$SRV_PID" 2>/dev/null || true; wait "$SRV_PID" 2>/dev/null || true; rm -rf "$TMP_DATA"' EXIT

GUIDB_ADMIN_PASSWORD= RUST_LOG=info GUIDB_DATA_DIR="$TMP_DATA" GUIDB_STATIC_DIR="$PWD/static" GUIDB_ADDR="127.0.0.1:0" \
  ./target/debug/guidb-server >"$TMP_DATA/server.log" 2>&1 &
SRV_PID=$!

for _ in $(seq 1 100); do
  addr=$(sed -n 's/.*"addr":"\(127\.0\.0\.1:[0-9]*\)".*/\1/p' "$TMP_DATA/server.log" | head -n1)
  [[ -n "$addr" ]] && break
  kill -0 "$SRV_PID" || exit 1
  sleep 0.1
done
base="http://${addr:?Server gagal berjalan}"

echo "1) GET / HTML & WASM static check"
curl -fsS "$base"/ | grep "GuiDB Studio" >/dev/null
curl -fsSI "$base"/static/guidb_wasm.wasm | grep "200 OK" >/dev/null
curl -fsSI "$base"/static/app.css | grep "200 OK" >/dev/null
curl -fsSI "$base"/static/app.js | grep "200 OK" >/dev/null

echo "2) Live SQLite query & staged commit persistence check"
curl -fsS -X POST "$base"/api/connections/conn-commerce-core/mutate \
  -H 'Content-Type: application/json' \
  -d '{"mutations":[{"op":"insert","item":"customers","values":{"company_name":{"kind":"text","value":"PT Verifikasi Nyata"},"contact_email":{"kind":"text","value":"verify@nyata.id"},"city":{"kind":"text","value":"Surabaya"},"tier":{"kind":"text","value":"ENTERPRISE"},"credit_limit":{"kind":"int","value":75000000},"created_at":{"kind":"text","value":"2026-11-18T12:00:00Z"}}}]}' | grep '"committed_count":1' >/dev/null

sqlite3 "$TMP_DATA/commerce_core.sqlite" "SELECT company_name, city FROM customers WHERE contact_email='verify@nyata.id';" | grep "PT Verifikasi Nyata|Surabaya" >/dev/null

echo "3) Embedded document aggregation & KV persistence check"
curl -fsS -X POST "$base"/api/connections/conn-telemetry-docs/query \
  -H 'Content-Type: application/json' \
  -d '{"item":"service_events","query":"[{\"$match\":{\"severity\":\"error\"}},{\"$group\":{\"_id\":\"$service\",\"events\":{\"$sum\":1}}}]"}' | grep "checkout-api" >/dev/null

curl -fsS -X POST "$base"/api/connections/conn-edge-kv/query \
  -H 'Content-Type: application/json' \
  -d '{"query":"SET e2e:token active EX 90"}' | grep "OK" >/dev/null
sqlite3 "$TMP_DATA/edge_kv.sqlite" "SELECT value_text, ttl_seconds FROM _guidb_kv WHERE key='e2e:token';" | grep "active|90" >/dev/null

echo "4) Live Schema Sync Diff check"
curl -fsS -X POST "$base"/api/schema-sync/diff \
  -H 'Content-Type: application/json' \
  -d '{"source_conn_id":"conn-commerce-core","target_conn_id":"conn-commerce-staging"}' | grep "shipments" >/dev/null

echo "ALL E2E CURL + SQLITE DB CHECKS PASSED"

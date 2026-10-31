#!/bin/sh
# One-time setup: RPC password secret (readable by uid 1000, which dashd, the app, the publisher and
# the bench run as), placeholders for the Worker's ingest and bench tokens, and a default .env.
# Safe to re-run: existing secrets and .env are kept.
set -eu
cd "$(dirname "$0")/.."
mkdir -p secrets
if [ ! -s secrets/rpc_password ]; then
  head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > secrets/rpc_password
  echo "generated secrets/rpc_password"
fi
# The Worker's INGEST_TOKEN secret; the publisher idles until it's filled in.
[ -e secrets/ingest_token ] || : > secrets/ingest_token
# R2 S3 credentials for the publisher (with R2_ENDPOINT in .env); empty, it uses the ingest token.
[ -e secrets/r2_access_key_id ] || : > secrets/r2_access_key_id
[ -e secrets/r2_secret_access_key ] || : > secrets/r2_secret_access_key
# The bench's token for the Worker's ingest; the bench idles until it's filled in.
[ -e secrets/bench_token ] || : > secrets/bench_token
# The Worker's STATE_TOKEN, for the app's database snapshots; the app won't start without it
# (unset STATE_TARGET in docker-compose.yml to run without snapshots).
[ -e secrets/state_token ] || : > secrets/state_token
chown 1000:1000 secrets/* 2>/dev/null || true
chmod 0400 secrets/rpc_password
chmod 0400 secrets/ingest_token secrets/bench_token secrets/state_token secrets/r2_access_key_id secrets/r2_secret_access_key
[ -e .env ] || cp .env.example .env
echo "ok: edit .env if needed, then: docker compose up -d --build"

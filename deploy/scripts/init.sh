#!/bin/sh
# One-time setup: RPC password secret (readable by uid 1000, which dashd, the app and cloudflared
# run as) and a default .env.
set -eu
cd "$(dirname "$0")/.."
mkdir -p secrets
if [ ! -s secrets/rpc_password ]; then
  head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > secrets/rpc_password
  echo "generated secrets/rpc_password"
fi
[ -e secrets/cloudflare_tunnel_token ] || : > secrets/cloudflare_tunnel_token
chown 1000:1000 secrets/* 2>/dev/null || true
chmod 0400 secrets/rpc_password
chmod 0600 secrets/cloudflare_tunnel_token
[ -e .env ] || cp .env.example .env
echo "ok: edit .env if needed, then: docker compose up -d --build"

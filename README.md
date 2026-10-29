# dash-nodes

The Dash network and masternode monitor behind **nodes.dashhq.org**. It's a from-scratch take on
mnowatch.org/user-agents and the old dashninja.pl:

- **Network crawl:** every 15 minutes it handshakes with every registered masternode, every node that
  answered in the past week, and new addresses from our dashd and other nodes' `getaddr` replies. From
  each it records the user agent, protocol, services, reported height and handshake time, plus
  country and hosting ASN (DB-IP Lite).
- **Masternode list:** the deterministic list from dashd, a live port check per masternode, version
  history, daily port-check uptime, and every payment back to DIP3 enforcement (`masternode payments`,
  backfilled newest first).
- **History:** per-crawl distributions are kept for 14 days and daily averages forever, so
  version adoption and network size can be charted over time.

One Rust binary (`dash-nodes serve`) runs the crawler, the dashd collectors, the JSON API and the static
site (`web/`, plain HTML and JS with no build step). It keeps one SQLite file per network in
`DATA_DIR`. The API is documented at `/api`.

## Running it

```sh
cd deploy
./scripts/init.sh                 # RPC password secret + .env
docker compose up -d --build      # dashd mainnet + testnet, the app on :8080
```

dashd needs `txindex=1` (`masternode payments` recomputes fees), which `deploy/dashd/dash.conf` sets.
A cold mainnet sync takes many hours; seeding the datadirs from another synced node with the same
dashd version and txindex is much faster (see Deploy notes).

Set `COMPOSE_PROFILES=tunnel` and put a Cloudflare tunnel token in `secrets/cloudflare_tunnel_token`
to publish through cloudflared, routing the hostname to `http://app:8080`. Or point any existing
tunnel or reverse proxy at the host's port 8080.

Useful while developing:

```sh
cargo test
cargo run -- probe 1.2.3.4:9999 --getaddr   # handshake one node
MAINNET_RPC=http://127.0.0.1:9998 RPC_PASSWORD=… cargo run -- serve
```

## Configuration (env)

| Variable | Default | |
|---|---|---|
| `MAINNET_RPC`, `TESTNET_RPC` | unset | dashd RPC URLs; a network runs only if its URL is set |
| `RPC_USER`, `RPC_PASSWORD_FILE` / `RPC_PASSWORD` | `dashnodes` | dashd RPC credentials |
| `DATA_DIR`, `WEB_DIR`, `LISTEN` | `./data`, `./web`, `0.0.0.0:8080` | |
| `CRAWL_INTERVAL` | `900` | seconds between crawl starts |
| `CRAWL_CONCURRENCY` | `128` | parallel handshakes |
| `CRAWL_IPV6` | `false` | also crawl IPv6 addresses (only if the host has IPv6 routing) |
| `GETADDR_PER_CRAWL` | `200` | peers asked for addresses per crawl (each at most every 6 h) |
| `PAYMENTS_FLOOR_MAINNET`, `PAYMENTS_FLOOR_TESTNET` | DIP3 enforcement height | lowest block to backfill payments to |
| `BACKFILL_PAUSE_MS` | `200` | pause between 100-block backfill batches |

## Deploy notes

Production runs in container the host (`dash-nodes`, 192.0.2.10, 8 cores / 12 GB / 150 GB on
`local storage`) on the host. The source is unpacked at `/opt/dash-nodes` (the deployed commit is in
`DEPLOYED_COMMIT`) and the stack runs from `/opt/dash-nodes/deploy`.

- **Redeploy:** `git archive` or tar the tree (without `target/`, `deploy/secrets`, `deploy/.env`),
  unpack it over `/opt/dash-nodes`, then `docker compose build app && docker compose up -d app`.
  Restarting the app abandons a crawl in progress; the next one starts right away.
- **Seeding dashd:** the datadirs were seeded on 2026-10-29 from a ZFS snapshot of another node's
  (the host) v24.0.0-rc.4, txindex=1 datadirs (`blocks chainstate indexes evodb llmq`, rsynced on the
  host host into `dash-nodes_dashd-{mainnet,testnet}` volumes, owner the mapped uid). Keep `DASHD_TAG` at or
  above the version that wrote them.
- **Public access:** Cloudflare serves `nodes.dashhq.org` through the remote-managed tunnel in the host
  (`cloudflared`), whose public hostname points at `http://192.0.2.10:8080`. The compose
  `tunnel` profile is the alternative if this stack should own its own tunnel.
- **Data:** SQLite in the `dash-nodes_app-data` volume (`mainnet.sqlite`, `testnet.sqlite`,
  `geo/`). Deleting a file rebuilds that network from scratch (history is lost).

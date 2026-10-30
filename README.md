# dash-nodes

The Dash network and masternode monitor behind **nodes.dashhq.org**. It's a from-scratch take on
mnowatch.org/user-agents and the old dashninja.pl:

- **Network crawl:** every 15 minutes it handshakes with every registered masternode, every node that
  answered in the past week, and new addresses from our dashd and other nodes' `getaddr` replies. From
  each it records the user agent, protocol, services, reported height and handshake time, plus
  city, country and hosting ASN (DB-IP Lite).
- **Every address network:** IPv4, IPv6, Tor v3 onion services, I2P and CJDNS. The crawler speaks
  `addrv2` (BIP155), so other nodes tell it about all of them; it reaches onion peers through Tor,
  I2P peers through i2pd, CJDNS through a cjdns node, and IPv6 directly when the host has a global
  IPv6 route (or through a SOCKS5 proxy). A network without a route is still counted as *known*,
  just not crawled; each address counts as one node, so a node on IPv4 and Tor counts twice.
- **Masternode list:** the deterministic list from dashd, a live port check per masternode, version
  history, daily port-check uptime, and every payment back to DIP3 enforcement (`masternode payments`,
  backfilled newest first).
- **Map:** every masternode (or every reachable node, counted per city) placed by the city its IP
  geolocates to (DB-IP City Lite), clustered, colored by port-check status, version or type, with the
  list's filters. It replaces the old map.dashhq.org / map.thepasta.org (`PastaPastaPasta/dash-mn-map`).
  MapLibre GL draws it on a self-hosted Natural Earth basemap (`tools/basemap.py`); nothing is loaded
  from third parties (see `web/assets/vendor/README.md`).
- **History:** per-crawl distributions are kept for 14 days and daily averages forever, so
  version adoption and network size can be charted over time.

One Rust binary (`dash-nodes serve`) runs the crawler, the dashd collectors, the JSON API and the static
site (`web/`, plain HTML and JS with no build step). It keeps one SQLite file per network in
`DATA_DIR`. The API is documented at `/api`.

## How it's served

The public site doesn't talk to the crawler host:

```
crawler host (no inbound)                          Cloudflare
dashd ×2 ─ app (crawler, SQLite, API :8080) ─ publisher ──PUT /_ingest──▶ Worker ─▶ R2 (dash-nodes-data)
                                                                          │
                                         nodes.dashhq.org ◀── static assets + /api/v1/* from R2
```

- `worker/` is a Cloudflare Worker that serves `web/` as static assets and answers `/api/v1/*` from
  documents in R2: the same JSON as the app's API (`worker/test/parity.mjs` checks this). History is
  one document per scope and dimension holding every range (1, 7, 14, 30, 90 and 365 days) at top 20,
  so `days` must be one of those and `top` at most 20; masternode details live in 64 shards; payments
  in one list per masternode.
- `worker/scripts/publish.mjs` (the `publisher` compose service) reads the app's API inside the
  compose network and uploads those documents to the Worker's authenticated `PUT /_ingest/<key>`:
  status every minute, the rest after each crawl, skipping unchanged ones.
- If the crawler host is down, the site keeps serving the last published data and says how old it is.

Deploying the Worker (needs a Cloudflare API token with Workers Scripts and R2 edit on the account,
and Workers Routes and DNS edit on the zone):

```sh
cd worker
npm ci
npx wrangler r2 bucket create dash-nodes-data      # once
npx wrangler secret put INGEST_TOKEN               # once; the same value goes in deploy/secrets/ingest_token
npm run deploy                                      # copies ../web into public/ and deploys
```

`npm run dev` runs it locally (with a local R2); `SOURCE=http://127.0.0.1:8080 TARGET=http://127.0.0.1:8787
INGEST_TOKEN=… node scripts/publish.mjs --once` fills it from a running app.

## Running it

```sh
cd deploy
./scripts/init.sh                 # RPC password secret + .env
docker compose up -d --build      # dashd mainnet + testnet, the app on :8080
```

dashd needs `txindex=1` (`masternode payments` recomputes fees), which `deploy/dashd/dash.conf` sets.
A cold mainnet sync takes many hours; seeding the datadirs from another synced node with the same
dashd version and txindex is much faster (see Deploy notes).

Put the Worker's ingest token in `secrets/ingest_token` and the `publisher` service starts pushing to
`PUBLISH_TARGET`; until then it idles.

Useful while developing:

```sh
cargo test
cargo run -- probe 1.2.3.4:9999 --getaddr   # handshake one node (prints non-IPv4 gossip)
TOR_PROXY=127.0.0.1:9050 cargo run -- probe <56 chars>.onion:9999
I2P_PROXY=127.0.0.1:4447 cargo run -- probe <52 chars>.b32.i2p:0
MAINNET_RPC=http://127.0.0.1:9998 RPC_PASSWORD=… cargo run -- serve
```

## Configuration (env)

| Variable | Default | |
|---|---|---|
| `MAINNET_RPC`, `TESTNET_RPC` | unset | dashd RPC URLs; a network runs only if its URL is set |
| `RPC_USER`, `RPC_PASSWORD_FILE` / `RPC_PASSWORD` | `dashnodes` | dashd RPC credentials |
| `DATA_DIR`, `WEB_DIR`, `LISTEN` | `./data`, `./web`, `0.0.0.0:8080` | |
| `CRAWL_INTERVAL` | `900` | seconds between crawl starts |
| `CRAWL_CONCURRENCY` | `128` | parallel handshakes per directly reached network (IPv4, IPv6; CJDNS at most 32) |
| `CRAWL_IPV6` | `auto` | `auto`: crawl IPv6 if the host has a global IPv6 route (checked every crawl) or `IPV6_PROXY` is set; `true`: always; `false`: never |
| `IPV6_PROXY` | unset | SOCKS5 proxy (`host:port`) for IPv6 peers, for hosts without IPv6 |
| `TOR_PROXY` | unset | Tor SOCKS5 proxy (`host:port`); onion peers are crawled only if set |
| `I2P_PROXY` | unset | I2P SOCKS5 proxy (i2pd's `socksproxy`, `host:port`); I2P peers are crawled only if set |
| `CJDNS` | `auto` | `auto`: crawl CJDNS (fc00::/8) if a local cjdns interface routes it; `true`: route exists but can't be detected (NAT, as in the compose stack); `false`: never |
| `TOR_CONCURRENCY`, `I2P_CONCURRENCY` | `16`, `8` | parallel handshakes through Tor and I2P |
| `GETADDR_PER_CRAWL` | `200` | peers asked for addresses per crawl (each at most every 6 h) |
| `PAYMENTS_FLOOR_MAINNET`, `PAYMENTS_FLOOR_TESTNET` | DIP3 enforcement height | lowest block to backfill payments to |
| `BACKFILL_PAUSE_MS` | `200` | pause between 100-block backfill batches |

Timeouts are per network: connect / handshake / getaddr are 5/10/10 s for IPv4, 10/10/10 s for
IPv6, 60/30/30 s for Tor, 90/60/60 s for I2P and 20/15/15 s for CJDNS. Direct and proxied networks
are crawled side by side, so Tor and I2P never hold up IPv4. Networks behind a proxy stop starting
new probes after 60% of `CRAWL_INTERVAL` (running ones finish); the rest wait for the next crawl and
don't count as unreachable. A masternode's port check uses its primary (first) core P2P address.

The compose stack (Docker Engine 28+, Compose 2.33+) adds these; only the app's port is published:

| Service | Address | |
|---|---|---|
| `tor` | 172.30.88.10:9050 on `overlay` | Tor client (Debian package), SOCKS only, no relay or exit |
| `i2pd` | 172.30.88.11:4447 (SOCKS), :7656 (SAM) on `overlay` | `purplei2p/i2pd`, firewalled, bandwidth class L, ≤200 transit tunnels, no console |
| `cjdns` (profile `cjdns`) | fd86:d4a5:1c3e:186::fe on `cjdns` | cjdns built from the upstream tag (`deploy/cjdns`, commit-checked), peers from `deploy/cjdns/peers.json` (hyperboria/peers, IPv4) plus cjdns' DNS seeds; NATs fc00::/8 out of its TUN |
| `cjdns-route` (profile `cjdns`) | host network | keeps the host route `fc00::/8 via` the cjdns container (removed when it stops) |
| `publisher` | `edge` | Node 24, `worker/scripts/publish.mjs`: the app's API → the Worker's ingest (token in `secrets/ingest_token`) |

Tor and i2pd run with no capabilities, read-only, on their own `overlay` network, so they can't
reach dashd's RPC on `core`.

Both dashds use Tor (`-onion`, `-listenonion=0`) and i2pd (`-i2psam`, `-i2pacceptincoming=0`), so
their address managers, which the crawler reads, keep onion and I2P peers. With `CJDNS=true` they
also get `-cjdnsreachable=1`. dashd, the app and cjdns share the IPv6-only `cjdns` network (ULA,
no IPv4), whose gateway is the host. On a host without IPv6, internet IPv6 from there
fails within about a second; on a host with IPv6, Docker's NAT66 makes it work.

## Deploy notes

The stack runs on any Docker host (Docker Engine 28+, Compose 2.33+); 8 cores, 12 GB RAM and about
150 GB of disk are comfortable for both networks with `txindex`. Unpack the source somewhere such as
`/opt/dash-nodes` (keeping the deployed commit in a `DEPLOYED_COMMIT` file helps) and run the stack
from its `deploy/` directory.

- **Redeploy:** `git archive` or tar the tree (without `target/`, `deploy/secrets`, `deploy/.env`),
  unpack it over the old one, then `docker compose build app && docker compose up -d app`.
  Restarting the app abandons a crawl in progress; the next one starts right away.
- **Seeding dashd:** a cold sync takes many hours. Copying `blocks chainstate indexes evodb llmq`
  from another synced node with the same dashd version and `txindex=1` into the
  `dash-nodes_dashd-{mainnet,testnet}` volumes (owned by the container's uid 1000, as mapped on
  your host) is much faster. Keep `DASHD_TAG` at or above the version that wrote them.
- **Public access:** the `dash-nodes` Worker serves the site from R2; the host only connects out to
  it (see How it's served). The app's port 8080 is for the local network, for debugging.
- **Tor, I2P, CJDNS:** `tor` and `i2pd` always run; `cjdns` and `cjdns-route` need
  `COMPOSE_PROFILES=cjdns` and `CJDNS=true` in `.env`, and a `/dev/net/tun` device. In an
  unprivileged LXC container that takes two lines in the container's config on its host:
  `lxc.cgroup2.devices.allow: c 10:200 rwm` and
  `lxc.mount.entry: /dev/net/tun dev/net/tun none bind,create=file`. Their state is in the
  `dash-nodes_{tor,i2pd,cjdns}-data` volumes (the cjdns volume holds its keys and fc address).
  Without a TUN device, leave the `cjdns` profile off and `CJDNS=false`.
- **Data:** SQLite in the `dash-nodes_app-data` volume (`mainnet.sqlite`, `testnet.sqlite`,
  `geo/`). Deleting a file rebuilds that network from scratch (history is lost).

## License

MIT, see [LICENSE](LICENSE). Vendored and third-party assets keep their own licenses (`web/assets/vendor/README.md`).

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
- **Map:** every masternode placed by the city its IP geolocates to (DB-IP City Lite), clustered,
  colored by port-check status, version or type, with the list's filters. It replaces the old
  map.dashhq.org / map.thepasta.org (`PastaPastaPasta/dash-mn-map`), whose hostnames now redirect to `/map`.
  Basemap tiles come from OpenStreetMap; Leaflet and Leaflet.markercluster are vendored in `web/assets/vendor`.
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

Tor and i2pd run with no capabilities, read-only, on their own `overlay` network, so they can't
reach dashd's RPC on `core`.

Both dashds use Tor (`-onion`, `-listenonion=0`) and i2pd (`-i2psam`, `-i2pacceptincoming=0`), so
their address managers, which the crawler reads, keep onion and I2P peers. With `CJDNS=true` they
also get `-cjdnsreachable=1`. dashd, the app and cjdns share the IPv6-only `cjdns` network (ULA,
no IPv4), whose gateway is the host. On a host without IPv6 (the host), internet IPv6 from there
fails within about a second; on a host with IPv6, Docker's NAT66 makes it work.

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
- **Tor, I2P, CJDNS (2026-10-30):** `tor`, `i2pd`, `cjdns` and `cjdns-route` run on the host with
  `COMPOSE_PROFILES=cjdns` and `CJDNS=true` in `.env`. cjdns needs `/dev/net/tun`, which the
  unprivileged CT gets from two lines in `the container's config file` on the host (backup of the old
  file: `a backup copy`):
  `lxc.cgroup2.devices.allow: c 10:200 rwm` and
  `lxc.mount.entry: /dev/net/tun dev/net/tun none bind,create=file`. Their state is in the
  `dash-nodes_{tor,i2pd,cjdns}-data` volumes (the cjdns volume holds its keys and fc address).
  Without a TUN device, leave the `cjdns` profile off and `CJDNS=false`.
- **Data:** SQLite in the `dash-nodes_app-data` volume (`mainnet.sqlite`, `testnet.sqlite`,
  `geo/`). Deleting a file rebuilds that network from scratch (history is lost).

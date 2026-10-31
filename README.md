# dash-nodes

The Dash network and masternode monitor behind **nodes.dashhq.org**. It's a from-scratch take on
mnowatch.org/user-agents and the old dashninja.pl:

- **Network crawl:** every 15 minutes it handshakes with every registered masternode, every node that
  answered in the past week, and new addresses from our dashd and other nodes' `getaddr` replies. From
  each it records the user agent, protocol, services, reported height and handshake time, plus
  city, country and hosting ASN (DB-IP Lite).
- **Every address network:** IPv4, IPv6, Tor v3 onion services, I2P and CJDNS. The crawler speaks
  `addrv2` (BIP155), so other nodes tell it about all of them; it reaches onion peers through Tor,
  I2P peers through i2pd, CJDNS through a cjdns node, and IPv6 directly when the host reaches the
  IPv6 internet, natively or through NAT66 (or through a SOCKS5 proxy). A network without a route is still counted as *known*,
  just not crawled; each address counts as one node, so a node on IPv4 and Tor counts twice.
- **Masternode list:** the deterministic list from dashd, a live port check per masternode, version
  history, daily port-check uptime, and every payment back to DIP3 enforcement (from each block's
  coinbase and the masternode list at its height, backfilled newest first).
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
dashd ×2 ─ app (crawler, SQLite, API :8080) ─ publisher ──S3 PUT──────────────▶ R2 (dash-nodes-data)
                                             bench ──PUT /_ingest──▶ Worker ─▶ R2       │
                                         nodes.dashhq.org ◀── Worker: static assets + /api/v1/* from R2
```

- `worker/` is a Cloudflare Worker that serves `web/` as static assets and answers `/api/v1/*` from
  documents in R2: the same JSON as the app's API (`worker/test/parity.mjs` checks this). History is
  one document per scope and dimension holding every range (1, 7, 14, 30, 90 and 365 days) at top 20,
  so `days` must be one of those and `top` at most 20; masternode details live in 64 shards; payments
  in one list per masternode.
- `worker/scripts/publish.mjs` (the `publisher` compose service) reads the app's API inside the
  compose network and uploads those documents straight to the R2 bucket over its S3 API
  (`R2_ENDPOINT` and the `r2_*` secrets): status every minute, the rest after each crawl, skipping
  unchanged ones. Without `R2_ENDPOINT` it uses the Worker's authenticated `PUT /_ingest/<key>`
  instead (that's how a local `wrangler dev` gets seeded), but in production every upload would then
  be a Worker request, and a full republish (a new host, or a payments backfill) is ~26k of them.
- Speed-test vantages upload through `/_ingest` with their own tokens, which the Worker limits to
  their own documents (R2 tokens can't be limited to a key prefix); that's a few dozen an hour.
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


`/api/v1/health` answers 200 while every network's last crawl and chain reading are under 45
minutes old and dashd answers, and 503 with what's wrong otherwise. Point an uptime check at it:
it catches a stopped crawler, a stuck publisher and a dashd that won't start alike.
## Running it

```sh
cd deploy
./scripts/init.sh                 # RPC password secret + .env
docker compose up -d --build      # dashd mainnet + testnet, the app on :8080
```

dashd runs pruned (`deploy/dashd/dash.conf`): the app needs the chain state, the masternode list and
recent blocks, not the whole chain or a txindex. A pruned node can only backfill payments as far as
its oldest block, so the full history comes from a snapshot (below) or from a run against an
unpruned node. A cold mainnet sync takes many hours; seeding the datadirs from another node with the
same dashd version is much faster (see Deploy notes).

The `publisher` service uploads straight to R2 when `.env` sets `R2_ENDPOINT` and
`secrets/r2_access_key_id` and `secrets/r2_secret_access_key` hold an R2 API token's S3 credentials
(Object Read & Write on the data bucket only, never the snapshot bucket); without `R2_ENDPOINT` it
pushes to `PUBLISH_TARGET`'s ingest with `secrets/ingest_token`. Until its secrets are filled in, it
idles. Secrets are bind mounts: after replacing a secret file, `docker compose up -d publisher`.

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
| `CRAWL_IPV6` | `auto` | `auto`: crawl IPv6 if it reaches the internet (a global IPv6 address, or a test connection through NAT66; checked every crawl) or `IPV6_PROXY` is set; `true`: always; `false`: never |
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
| `publisher` | `edge` | Node 24, `worker/scripts/publish.mjs`: the app's API → R2 over S3 (`R2_ENDPOINT`, `secrets/r2_*`) |
| `bench` | `edge`, `overlay`, `cjdns` | the app image running `dash-nodes bench` (vantage from `BENCH_VANTAGE`): masternode speed tests → the Worker's ingest (token in `secrets/bench_token`) |

Tor and i2pd run with no capabilities, read-only, on their own `overlay` network, so they can't
reach dashd's RPC on `core`.

Both dashds use Tor (`-onion`, `-listenonion=0`) and i2pd (`-i2psam`, `-i2pacceptincoming=0`), so
their address managers, which the crawler reads, keep onion and I2P peers. With `CJDNS=true` they
also get `-cjdnsreachable=1`. dashd, the app and cjdns share the IPv6-only `cjdns` network (ULA,
no IPv4), whose gateway is the host. On a host without IPv6, internet IPv6 from there
fails within about a second; on a host with IPv6, Docker's NAT66 makes it work.

## Masternode speed tests (bench)

`dash-nodes bench` tests every enabled masternode about once every two weeks from each vantage
point and publishes a report per masternode. It needs no dashd: the masternode list and reference
block hashes come from `SOURCE` (the app's API, or nodes.dashhq.org), so the same image runs in the
compose stack and on small boxes elsewhere (`deploy/bench-vantage/`), each under its own vantage id.

A test, over the masternode's core P2P address (`service`), as an ordinary light client (it answers
the node's pings and ignores everything else it isn't waiting for), bounded at 120 s:

1. TCP connect and the version/verack handshake (user agent, protocol, start height).
2. Five `ping`s, one at a time: min, median, max round trip.
3. The newest 50,000 headers: `getheaders` from the reference hash at `tip − 50,000`, 2,000 per
   reply, up to the reference tip (or the node's advertised height, if lower). Each header must link
   to the one before (X11 hash) and every 2,000th must match our dashd's hash; only headers up to
   the last matching one count as served. 10 s without a byte from the node is a stall and the
   request is re-sent (at most 3 times). Headers per second count only time spent waiting on the node.
4. `getmnlistd` from the empty list (base hash zero) to the reference tip: time, size, and a check
   that it parses (header, coinbase and every list entry), is for that block and has about as many
   entries as the list.
5. A grade from spv-network-health's rubric (QuantumExplorer, MIT; see `src/bench/grade.rs`):
   completeness 45%, throughput 20% (full marks at 2,500 headers/s), reliability 20%, latency 10%
   (median ping), connectivity 5%, with incomplete downloads capped at C/D/F.

A node that can't be connected to or doesn't finish the handshake gets `"ok": false` and no grade.

**Pacing.** A masternode is due again a cycle after its last successful test, or 12 h, 24 h, 48 h …
(up to a cycle) after a failure. Starts are spaced so a full pass plus the retries of the
masternodes currently failing (4 each) fits in 90% of `CYCLE_DAYS`, jittered ±50%, at most
`BENCH_CONCURRENCY` at once; while any masternode is more than a tenth of a cycle overdue (after
downtime), starts come twice as fast. No two tests run in the same IPv4 /16 or IPv6 /32 at once,
and no new test starts while running ones download more than `BENCH_MAX_MBIT`. Results (the newest 6 per masternode) live in `STATE_DIR/bench.sqlite`.
For mainnet's ~2,250 enabled masternodes plus testnet's ~100 that's a start every ~7.7 min, a pass
in 12.6 days, and per test about 4.6 MB downloaded (4.05 MB of headers, 0.5 MB of masternode list),
so ~11 GB per vantage per cycle. `--sweep` tests everything not tested this cycle right away (8 at a
time, about an hour for mainnet, ~23 Mbit/s); `--once <addr>` tests one node and prints the result.

**Reference hashes:** `GET /api/v1/{net}/reference` (the app; the publisher uploads it as
`v1/{net}/reference.json`): `{"tip": H, "tip_hash": "…", "hashes": {"<height>": "<hash>", …}}` for
`tip − 50,000` … `tip` every 2,000 blocks, in RPC byte order. `tip` is our dashd's tip minus 6
blocks, refreshed when it has moved 100 blocks; 503 until dashd is synced.

**Published documents** (every 10 minutes and at the end of a sweep, changed ones only, plus
everything once a day; to `PUT {TARGET}/_ingest/<key>` with `Authorization: Bearer <token>`, gzip
body, `x-body-encoding: gzip`; a round stops at the first failed upload and resumes next time):

- `v1/{net}/bench/{vantage}/{shard}.json`: `{"<protx>": [result, …]}`, newest first, at most 6 per
  masternode; `shard` = (first ProTx byte × 64) >> 8 as two hex digits (`00`–`3f`, like the Worker's
  `shardOf`). All 64 are published, `{}` when empty.
- `v1/{net}/bench/{vantage}/summary.json`: `{"vantage", "location", "cycle_days", "updated"` (time of
  the newest result, null before the first)`, "tested"` (masternodes with a result this cycle)`,
  "scores", "headers_per_sec", "ping_ms"` (latest per masternode, ascending; failed tests are left
  out)`, "grades": {"A": n, …, "F": n}, "failed"}`.
- A result: `{"ts", "vantage", "address", "ok", "error", "user_agent", "protocol", "height",
  "connect_ms", "handshake_ms", "ping_ms": {"min", "median", "max", "samples"}, "headers":
  {"start_height", "requested", "served", "valid", "seconds", "per_sec", "bytes", "stalls"},
  "mnlistdiff": {"bytes", "seconds", "entries", "ok"}, "grade": {"score", "letter", "completeness",
  "throughput", "reliability", "latency", "connectivity"}}`. Times in ms or s; anything not measured
  is null (`grade` when `ok` is false; `latency` when no pong came back). `error` lists what went
  wrong, phases separated by `; `, even when the test was graded (e.g. a mnlistdiff timeout).

| Variable | Default | |
|---|---|---|
| `VANTAGE`, `VANTAGE_LOCATION` | required, empty | vantage id (`[a-z0-9-]{2,16}`) and where it is, for people (in the compose stack: `BENCH_VANTAGE`, `BENCH_LOCATION`) |
| `SOURCE` | `https://nodes.dashhq.org` | masternode lists and reference hashes (the app's API or the Worker) |
| `TARGET`, `BENCH_TOKEN` / `BENCH_TOKEN_FILE` | | the Worker and its ingest token; without a token the bench waits (re-reading the file every minute) |
| `NETWORKS`, `CYCLE_DAYS` | `mainnet testnet`, `14` | |
| `BENCH_CONCURRENCY`, `BENCH_SWEEP_CONCURRENCY` | `2`, `8` | tests at once, paced and with `--sweep` |
| `BENCH_MAX_MBIT` | `24` | don't start tests above this download rate |
| `BENCH_DEPTH`, `BENCH_STALL_SECS`, `BENCH_TEST_TIMEOUT`, `BENCH_PINGS` | `50000`, `10`, `120`, `5` | |
| `BENCH_PUBLISH_INTERVAL`, `STATE_DIR` | `600`, `./bench-state` (`/state` in the image) | |

The transports are the crawler's (`TOR_PROXY`, `I2P_PROXY`, `CRAWL_IPV6`, `IPV6_PROXY`, `CJDNS`);
masternodes on a network without a route are skipped, not failed.

**Adding a vantage.** Each vantage writes only its own documents, with its own token. Generate a
random token, give the vantage the token itself (`BENCH_TOKEN_FILE`), and give the Worker its
SHA-256 in lowercase hex, plus the vantage id in the list the Worker combines:

```sh
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > bench_token     # for the vantage
printf '%s' "$(cat bench_token)" | sha256sum | cut -d' ' -f1         # for BENCH_TOKENS
cd worker && npx wrangler secret put BENCH_TOKENS   # {"local":"<sha256>","nyc":"<sha256>",…}
# and add the id to BENCH_VANTAGES in wrangler.jsonc ("local,nyc,…"), then npm run deploy
```

The Worker answers `/api/v1/{net}/masternodes/{protx}/bench` (every vantage's runs, each one's rank
among the masternodes it tested and the average of their latest grades; a vantage whose newest run
failed shows the failure and counts for neither) and `/api/v1/{net}/bench` (each vantage's summary).

```sh
SOURCE=http://127.0.0.1:8080 cargo run --release -- bench --once 1.2.3.4:9999
# Without TARGET a sweep only keeps results in STATE_DIR. Never publish as another vantage's id:
# its documents would replace that vantage's.
SOURCE=http://127.0.0.1:8080 VANTAGE=dev STATE_DIR=/tmp/b cargo run --release -- bench --sweep --limit 20
```

## Deploy notes

The stack runs on any Docker host (Docker Engine 28+, Compose 2.33+); 2 cores, 4 GB RAM and 80 GB
of disk are enough for both networks pruned. Unpack the source somewhere such as
`/opt/dash-nodes` (keeping the deployed commit in a `DEPLOYED_COMMIT` file helps) and run the stack
from its `deploy/` directory.

- **Continuous deployment** (`deploy/cd/`): a merge to `main` deploys itself. The host pulls; nothing
  connects in, and no deploy key is on GitHub. Every 5 minutes `dash-nodes-update.timer` runs
  `update.sh`, which fetches `main` and, if it moved:
  1. checks that the new head is signed by a trusted key (git's `%G?` must be `G`) and descends from
     the deployed commit. Trusted: the SSH signing keys in `/etc/dash-nodes-cd/allowed_signers`, and
     GitHub's web-flow GPG key (it signs squash merges made on GitHub, so trusting it means trusting
     whoever can merge on the repo) in `/etc/dash-nodes-cd/gnupg`. Anything else is refused and logged;
  2. unpacks the tree over `/opt/dash-nodes`, runs `scripts/init.sh`, `docker compose build` and
     `up -d`, and restarts services whose bind-mounted files changed (the publisher's scripts, dashd's
     and i2pd's config). Then it checks that the app's `/healthz` answers from the new image and still
     does a minute later, and that every service that was running still is. If not, the last good
     tree and images (tagged `:good`; `:previous` is the one before) go back. A container that
     restarts during that minute counts as a failure too. A commit is tried 3 times
     (a pass that dies mid-deploy counts) and then waits for a newer one. Docs-only changes just
     update the tree;
  3. if what the Worker is built from changed (`web/`, `worker/src/`, `worker/scripts/build.mjs`,
     `worker/package*.json`, `worker/wrangler.jsonc`; not the publisher's scripts), deploys it: `npm ci --ignore-scripts`, `npm test` and
     the build in a `node` container without the token, then `wrangler deploy` with it (wrangler
     itself is pinned by the lockfile like everything else). It waits until every network's
     published summary comes from a crawl that finished after the new app started, so pages never
     ask for fields the data doesn't have yet. The token (one that can edit Workers scripts) lives in
     `/etc/dash-nodes-cd/cf-token` (0600); without it, each pass logs that the Worker is waiting.

  Install as root on the host, from the deployed tree: `sh deploy/cd/install.sh`, then check the
  fingerprints it prints and `sh deploy/cd/install.sh --enable` (the first pass runs right away). It
  writes `/etc/dash-nodes-cd/env` (host settings, from `deploy/cd/env.example`), fetches the trust
  files (the SSH keys from GitHub as of install time, and the pinned web-flow key), mirrors the repo
  into `/var/lib/dash-nodes-cd`, and records what's deployed from `DEPLOYED_COMMIT`, or from
  `APP_COMMIT=… WORKER_COMMIT=…` (full or short hashes) when the live Worker is from another commit.
  It also tags the running images `:good`, and refuses an `/opt/dash-nodes` that anyone but root
  can write. Edit the trust files by hand afterwards; nothing refreshes them. A deploy that changes
  `deploy/cd/` re-installs the updater.

  ```sh
  dash-nodes-update status                  # deployed app/Worker commits, main, pause, failures
  journalctl -u dash-nodes-update -f        # what each pass did
  touch /etc/dash-nodes-cd/paused           # pause (rm to resume); or systemctl disable --now dash-nodes-update.timer
  dash-nodes-update deploy <commit>         # deploy a signed commit now: a rollback, or a retry
  ```

  **Rolling back:** `dash-nodes-update deploy <last good commit>`. Going back, the Worker is deployed
  first and without waiting, since older pages work on newer data. Passes then skip `main`'s head
  as it was at that moment, and deploy again once `main` moves on (the fix). The installed updater
  stays as it is: a deploy only replaces it with one from a newer commit. The schema migrations so far
  only add nullable columns, so an older app runs on a newer database. For an instant app-only
  rollback: pause, then `docker tag dash-nodes:previous dash-nodes:local && docker compose up -d app bench`.
  The tree and `app-commit` then still say the newer commit, so stay paused until `deploy` sorts it
  out. `npx wrangler rollback` (from `worker/`, with a token) puts back the previous Worker version.
- **Bench vantages on other hosts** (`deploy/bench-vantage/`) don't update themselves: a 1 GB box can't
  build the image, and they take no inbound connections. When a deploy changes the bench code
  (`src/bench/`, `src/p2p.rs`, `src/transport.rs`), the updater logs a reminder; copy the image over
  by hand as described there. The compose stack's own `bench` service runs the app image, so it
  updates with the app.
- **Redeploy by hand** (the fallback, when CD is paused or the host has none): `git archive` or tar
  the tree (without `target/`, `deploy/secrets`, `deploy/.env`), unpack it over the old one, run
  `scripts/init.sh` (it adds any new secret placeholders; compose won't start a service whose secret
  file is missing), then `docker compose build && docker compose up -d`, and `docker compose restart`
  every service whose bind-mounted files changed (running containers keep the old ones): the
  publisher for `worker/src` and `worker/scripts`, the dashds for `deploy/dashd/`, i2pd for
  `deploy/i2pd/`, cjdns for `deploy/cjdns/peers.json`. Then update `DEPLOYED_COMMIT`. When the app's API gained fields the pages use, wait for the publisher
  to upload a crawl from the new app before deploying the Worker (`cd worker && npm run deploy`;
  plain `wrangler deploy` skips copying `web/` and ships stale pages). With CD installed, record a hand
  deploy in `/var/lib/dash-nodes-cd/app-commit` and `worker-commit` (full hashes), or CD will
  redeploy over it. Restarting the app abandons a crawl in progress; the next one starts right away.
- **Seeding dashd:** a cold sync takes many hours. Copying `blocks chainstate evodb llmq` (and
  `indexes` without `txindex`) from another synced node with the same dashd version into the
  `dash-nodes_dashd-{mainnet,testnet}` volumes (owned by the container's uid 1000, as mapped on
  your host) is much faster. Keep `DASHD_TAG` at or above the version that wrote them.
- **Public access:** the `dash-nodes` Worker serves the site from R2; the host only connects out to
  R2 and the Worker (see How it's served). The app's port 8080 is for the local network, for debugging.
- **Tor, I2P, CJDNS:** `tor` and `i2pd` always run; `cjdns` and `cjdns-route` need
  `COMPOSE_PROFILES=cjdns` and `CJDNS=true` in `.env`, and a `/dev/net/tun` device. In an
  unprivileged LXC container that takes two lines in the container's config on its host:
  `lxc.cgroup2.devices.allow: c 10:200 rwm` and
  `lxc.mount.entry: /dev/net/tun dev/net/tun none bind,create=file`. Their state is in the
  `dash-nodes_{tor,i2pd,cjdns}-data` volumes (the cjdns volume holds its keys and fc address).
  Without a TUN device, leave the `cjdns` profile off and `CJDNS=false`.
- **Bench (2026-10-30):** the `bench` service runs from the app image (`docker compose up -d app
  bench` after a build); its results are in the `dash-nodes_bench-state` volume. It idles until
  `secrets/bench_token` (created empty by `init.sh`) holds the Worker's bench ingest token, and
  re-reads it every minute. Write the token into the existing file (`cat > secrets/bench_token`): the
  secret is a bind mount, so a file replaced by a new one needs `docker compose up -d bench`.
- **DNS seeds:** dashd finds its first peers through DNS seeds (`dnsseed.dash.org` on mainnet).
  Some cloud providers' resolvers answer SERVFAIL for it (seen on DigitalOcean, 2026-10-30), which
  leaves a fresh mainnet node with no peers. Give Docker public resolvers in
  `/etc/docker/daemon.json`: `{"dns": ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"]}`.
- **Snapshots: the host holds nothing that can't be rebuilt.** The app snapshots its databases to
  the Worker's private `dash-nodes-state` bucket (`/_state/`, the STATE_TOKEN secret in `secrets/state_token`; every
  `STATE_INTERVAL`, 6 h, and 10 minutes after a start), and the bench its results (its own token,
  every 3 h). On a new host they restore themselves before anything else runs: a database file that
  doesn't exist yet is fetched from the latest snapshot. If one exists but can't be fetched, they
  stop rather than start empty. Everything else (dashd, Tor and I2P state, the publisher's record of
  what it uploaded) is re-derived.
- **Data:** SQLite in the `dash-nodes_app-data` volume (`mainnet.sqlite`, `testnet.sqlite`,
  `geo/`). Deleting a file brings it back from the latest snapshot (see Snapshots); without snapshots it rebuilds that network from scratch (history is lost).

## License

MIT, see [LICENSE](LICENSE). Vendored and third-party assets keep their own licenses (`web/assets/vendor/README.md`).

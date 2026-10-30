# A bench vantage on its own box

Runs `dash-nodes bench` (see "Masternode speed tests" in the top-level README) on a small Linux box,
for example a DigitalOcean droplet in NYC, Amsterdam or Singapore, so masternodes are tested from
several places. It reads the masternode list and reference hashes from the public site and
publishes its results under its own vantage id; nothing connects to it.

**Size:** 1 vCPU / 1 GB (a $6/month droplet) is plenty: a test uses about a second of CPU (X11
hashing) and a few MB of memory. Traffic is about 11 GB downloaded per 14-day cycle for mainnet
plus testnet (4.6 MB per masternode), well inside any droplet's allowance. Enable IPv6 on the
droplet so masternodes with IPv6 addresses can be tested too. No inbound ports: firewall everything
but SSH.

## Image

Build it from this repo where there's memory for a Rust release build (not on a 1 GB droplet), then
copy it over:

```sh
docker build -t dash-nodes:local .                      # in the repo root
docker save dash-nodes:local | gzip | ssh root@<droplet> 'gunzip | docker load'
```

or set `BENCH_IMAGE` to a registry image built from the same Dockerfile.

## Setup

```sh
# on the droplet (Docker Engine + compose plugin installed), with this directory copied over:
cp .env.example .env                 # VANTAGE (nyc, ams, sgp, ...) and VANTAGE_LOCATION
install -m 0400 -o 1000 -g 1000 /dev/null bench_token
cat > bench_token                    # paste the Worker's bench ingest token, then Ctrl-D
docker compose up -d
docker compose logs -f bench
```

Without a token the bench waits quietly, re-reading `bench_token` every minute (write into the
existing file; a replaced file needs `docker compose up -d`). Its state (the last results per
masternode, what was uploaded) is in the `bench-state` volume, so restarts and image updates neither
re-test nor re-upload.

For a first fill instead of waiting a cycle (8 tests at a time, about an hour for mainnet; stop the
paced service first so the two don't pick the same masternodes):

```sh
docker compose stop bench && docker compose run --rm bench bench --sweep && docker compose up -d
```

To test one node by hand: `docker compose run --rm bench bench --once 1.2.3.4:9999`.

Onion and I2P masternodes are skipped here (no Tor or I2P client); a vantage with both, such as
the main compose stack, tests them.

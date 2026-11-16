//! Periodic crawl of the P2P network: handshake every known address, ask some peers for more,
//! record what each node announces, then snapshot the distributions for the charts.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::StreamExt;
use rand::Rng;
use rand::SeedableRng;
use rand::seq::{IndexedRandom, SliceRandom};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::addr::{AddrNet, PeerAddr, net_of};
use crate::collector::pose_status;
use crate::db::{self, day_of, get_meta, query_all, set_meta};
use crate::mnauth::MnAuth;
use crate::network::Network;
use crate::p2p::{self, Announce, Probe};
use crate::state::NetCtx;
use crate::transport::{Route, Routes};
use crate::ua;

const DAY: i64 = 86_400;
/// A node that answered within this window still counts as part of the network when it misses a crawl.
pub const ACTIVE_WINDOW: i64 = 7 * DAY;
const GETADDR_EVERY: i64 = 6 * 3600;
/// Addresses that never answered get this many tries (with backoff) before going to weekly retries.
const NEW_ADDR_TRIES: i64 = 6;
/// Never-reached addresses probed per crawl, per pass. The rest wait for the next crawl.
const MAX_UNPROVEN_PER_CRAWL: usize = 5_000;
/// Share of the crawl interval that networks reached through a proxy (Tor, I2P) may take. Probes
/// not started by then wait for the next crawl, so a slow overlay never stalls the crawl.
const PROXIED_BUDGET_SHARE: f64 = 0.6;
/// The first crawl asks this many peers for addresses, to bootstrap the address table.
const BOOTSTRAP_GETADDR: usize = 1_000;
const PER_CRAWL_RETENTION: i64 = 14 * DAY;
const MN_DAILY_RETENTION: i64 = 180 * DAY;
const NODE_RETENTION: i64 = 30 * DAY;
/// How often each node hears our onion or I2P address: about as often as Dash Core announces its
/// own address to a peer (AVG_LOCAL_ADDRESS_BROADCAST_INTERVAL).
const ANNOUNCE_EVERY: i64 = DAY;

pub async fn run(ctx: Arc<NetCtx>) {
    // Pick up where the last process left off, so status doesn't read "never crawled" after a restart.
    let last = ctx
        .db
        .read(|c| {
            Ok(c.query_row(
                "SELECT id, finished FROM crawls WHERE finished IS NOT NULL ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?)
        })
        .await
        .unwrap_or(None);
    {
        let mut s = ctx.crawl_status.write().unwrap();
        s.running = false;
        if let Some((id, finished)) = last {
            s.last_id = Some(id);
            s.last_finished = Some(finished);
        }
    }
    while !ctx.mn_loaded.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    loop {
        let started = Instant::now();
        ctx.crawl_status.write().unwrap().running = true;
        let result = crawl_once(&ctx).await;
        {
            let mut s = ctx.crawl_status.write().unwrap();
            s.running = false;
            match &result {
                Ok(id) => {
                    s.last_id = Some(*id);
                    s.last_finished = Some(db::now());
                    s.last_error = None;
                }
                Err(e) => s.last_error = Some(format!("{e:#}")),
            }
        }
        match result {
            Ok(id) => tracing::info!(net = ctx.net.name(), "crawl {id} done in {:.0?}", started.elapsed()),
            Err(e) => tracing::warn!(net = ctx.net.name(), "crawl failed: {e:#}"),
        }
        if let Err(e) = ctx.db.write(maintenance).await {
            tracing::warn!(net = ctx.net.name(), "maintenance: {e:#}");
        }
        let interval = Duration::from_secs(ctx.crawl_cfg.interval_secs);
        tokio::time::sleep(interval.saturating_sub(started.elapsed())).await;
    }
}

/// What a probe does besides the handshake.
#[derive(Clone, Copy, Default)]
struct Ask {
    getaddr: bool,
    announce: Option<Announce>,
}

/// A `nodes` row due for a probe.
struct Due {
    addr: String,
    last_success: Option<i64>,
    last_getaddr: Option<i64>,
    last_announce: Option<i64>,
}

struct Outcome {
    addr: PeerAddr,
    asked_addrs: bool,
    result: Result<Probe>,
}

async fn crawl_once(ctx: &NetCtx) -> Result<i64> {
    let now = db::now();
    let started = Instant::now();
    // Decided once per crawl: a proxy or CJDNS router that appears later counts from the next one.
    let routes = ctx.crawl_cfg.transport.routes();
    learn_from_dashd(ctx).await?;
    let beacon = beacon(ctx).await;

    let id = ctx
        .db
        .write(move |c| {
            c.execute("INSERT INTO crawls (started) VALUES (?1)", [now])?;
            Ok(c.last_insert_rowid())
        })
        .await?;

    let (mn_addrs, due) = ctx
        .db
        .read(move |c| {
            // Every core P2P address of every masternode, on any network.
            let mn: Vec<String> = query_all(c, "SELECT DISTINCT addr FROM mn_addrs", [], |r| r.get(0))?;
            let due: Vec<Due> = query_all(
                c,
                "SELECT addr, last_success, last_getaddr, last_announce FROM nodes WHERE next_try <= ?1",
                [now],
                |r| {
                    Ok(Due {
                        addr: r.get(0)?,
                        last_success: r.get(1)?,
                        last_getaddr: r.get(2)?,
                        last_announce: r.get(3)?,
                    })
                },
            )?;
            Ok((mn, due))
        })
        .await?;

    let first_crawl = ctx
        .db
        .read(|c| {
            Ok(c.query_row("SELECT count(*) FROM crawls WHERE finished IS NOT NULL", [], |r| r.get::<_, i64>(0))? == 0)
        })
        .await?;
    let mut rng = rand::rngs::StdRng::from_rng(&mut rand::rng());
    // Masternode addresses count as known on every network, crawled or not.
    let mn_addrs: Vec<PeerAddr> =
        mn_addrs.iter().filter_map(|s| s.parse().ok()).filter(PeerAddr::is_routable).collect();
    upsert_addrs(ctx, mn_addrs.clone(), "mnlist").await?;
    // Addresses on networks we can't reach right now stay in `nodes` untouched: known, not crawled.
    let usable = |a: &PeerAddr| routes.for_addr(a).is_some();
    let mut targets: HashMap<PeerAddr, Ask> = HashMap::new();
    for a in mn_addrs.into_iter().filter(usable) {
        targets.insert(a, Ask::default());
    }
    // Ask nodes that answered recently for addresses, a random sample each crawl.
    let mut getaddr_candidates = Vec::new();
    let mut unproven = Vec::new();
    for Due { addr, last_success, last_getaddr, last_announce } in &due {
        let Ok(a) = addr.parse::<PeerAddr>() else {
            continue;
        };
        if !usable(&a) {
            continue;
        }
        match last_success {
            Some(t) if now - t < ACTIVE_WINDOW => {
                targets.entry(a).or_default().announce = pick_announce(&beacon, *last_announce, now, &mut rng);
                if last_getaddr.is_none_or(|t| now - t > GETADDR_EVERY) {
                    getaddr_candidates.push(a);
                }
            }
            _ => unproven.push(a),
        }
    }
    unproven.shuffle(&mut rng);
    unproven.truncate(MAX_UNPROVEN_PER_CRAWL);
    for a in &unproven {
        targets.entry(*a).or_default();
    }
    // Bootstrap: with nothing heard yet, ask a sample of whoever we have.
    let limit = if first_crawl {
        getaddr_candidates = targets.keys().copied().collect();
        BOOTSTRAP_GETADDR
    } else {
        ctx.crawl_cfg.getaddr_per_crawl
    };
    getaddr_candidates.shuffle(&mut rng);
    for a in getaddr_candidates.into_iter().take(limit) {
        targets.entry(a).or_default().getaddr = true;
    }

    let tip = ctx.status.read().unwrap().blocks as i32;
    let budget = Duration::from_secs_f64(ctx.crawl_cfg.interval_secs.max(60) as f64 * PROXIED_BUDGET_SHARE);
    let deadline = started + budget;
    // Direct networks and proxied ones (Tor, I2P) run side by side, so a slow overlay never holds
    // up IPv4. Known-good addresses go first, so a proxied network that runs out of time skips
    // new ones.
    let proxied = |a: &PeerAddr| matches!(routes.for_addr(a), Some(Route::Socks5(_)));
    let unproven: HashSet<PeerAddr> = unproven.into_iter().collect();
    let (mut proxied_first, mut direct_first): (Vec<_>, Vec<_>) =
        targets.iter().map(|(a, g)| (*a, *g)).partition(|(a, _)| proxied(a));
    for list in [&mut direct_first, &mut proxied_first] {
        list.sort_by_key(|(a, _)| unproven.contains(a));
    }

    // Each side then makes a second pass over addresses it just learned about, so this crawl's
    // picture is complete. The proxied side also takes what direct peers gossiped (that's where
    // most onion addresses come from); the direct side doesn't wait for the proxied one.
    let seen: HashSet<PeerAddr> = targets.keys().copied().collect();
    let (direct_gossip_tx, direct_gossip_rx) = tokio::sync::oneshot::channel();
    let direct_run = async {
        let mut out = probe_all(ctx, &routes, direct_first, tip, None).await;
        let gossip = take_gossip(ctx, &mut out);
        let _ = direct_gossip_tx.send(gossip.clone());
        let (fresh, found) = fresh_targets(&gossip, &seen, |a| usable(a) && !proxied(a));
        out.extend(probe_all(ctx, &routes, fresh, tip, None).await);
        (out, gossip, found)
    };
    let proxied_run = async {
        let mut out = probe_all(ctx, &routes, proxied_first, tip, Some(deadline)).await;
        let gossip = take_gossip(ctx, &mut out);
        let mut all = direct_gossip_rx.await.unwrap_or_default();
        all.extend_from_slice(&gossip);
        let (fresh, found) = fresh_targets(&all, &seen, proxied);
        out.extend(probe_all(ctx, &routes, fresh, tip, Some(deadline)).await);
        (out, gossip, found)
    };
    let ((mut outcomes, mut gossip, direct_found), (proxied_out, proxied_gossip, proxied_found)) =
        futures::join!(direct_run, proxied_run);
    outcomes.extend(proxied_out);
    gossip.extend(proxied_gossip);
    let discovered = direct_found + proxied_found;
    upsert_addrs(ctx, gossip, "getaddr").await?;

    let tried = outcomes.len();
    let rows: Vec<ProbeRow> = outcomes.into_iter().map(|o| ProbeRow::from(o, &ctx.geo)).collect();
    let tip = tip as i64;
    ctx.db.write(move |c| record(c, id, now, tip, tried, discovered, rows, &routes)).await.context("record crawl")?;
    Ok(id)
}

/// Whether an address learned from a peer or dashd is worth recording: routable, on the network's
/// default port (port 0 for I2P). Anything else could aim us at arbitrary services on third-party
/// hosts; masternodes on other ports come from the list.
fn is_trusted_gossip(net: Network, a: &PeerAddr) -> bool {
    a.is_routable() && a.port == net.default_port_for(a.net())
}

/// Our dashd's onion and I2P addresses, when it accepts connections there (deploy/dashd), to
/// announce to the nodes we probe. Dash Core never tells a peer its own onion or I2P address unless
/// the peer reached it on that network, so without a seed or a ProTx listing one, nobody hears of
/// any (testnet had none). Ours seed them: nodes that use Tor or I2P connect to our dashd there and
/// announce their own addresses to it, which learn_from_dashd picks up.
async fn beacon(ctx: &NetCtx) -> Vec<Announce> {
    match ctx.rpc.call::<Value>("getnetworkinfo", json!([])).await {
        Ok(info) => beacon_from(ctx.net, &info),
        Err(e) => {
            tracing::warn!(net = ctx.net.name(), "getnetworkinfo: {e:#}");
            Vec::new()
        }
    }
}

fn beacon_from(net: Network, info: &Value) -> Vec<Announce> {
    let Some(services) = info["localservices"].as_str().and_then(|s| u64::from_str_radix(s, 16).ok()) else {
        return Vec::new();
    };
    info["localaddresses"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| PeerAddr::from_parts(e["address"].as_str()?, u16::try_from(e["port"].as_u64()?).ok()?))
        .filter(|a| matches!(a.net(), AddrNet::Onion | AddrNet::I2p) && is_trusted_gossip(net, a))
        .map(|addr| Announce { addr, services })
        .collect()
}

/// One of our addresses for a node that hasn't heard one in ANNOUNCE_EVERY. Dash Core takes only
/// one from a peer it just met, so each probe announces one, picked at random.
fn pick_announce(beacon: &[Announce], last: Option<i64>, now: i64, rng: &mut impl Rng) -> Option<Announce> {
    if last.is_some_and(|t| now - t < ANNOUNCE_EVERY) {
        return None;
    }
    beacon.choose(rng).copied()
}

/// Takes the trusted gossiped addresses out of successful probes. Addresses on networks we can't
/// reach are kept too, as known.
fn take_gossip(ctx: &NetCtx, outcomes: &mut [Outcome]) -> Vec<PeerAddr> {
    let mut gossip = Vec::new();
    for o in outcomes {
        if let Ok(p) = &mut o.result {
            gossip.extend(std::mem::take(&mut p.addrs).into_iter().filter(|a| is_trusted_gossip(ctx.net, a)));
        }
    }
    gossip
}

/// Second-pass targets: gossiped addresses `keep` accepts that this crawl hasn't probed, a random
/// sample of at most MAX_UNPROVEN_PER_CRAWL. Also returns how many there were before sampling.
fn fresh_targets(
    gossip: &[PeerAddr],
    seen: &HashSet<PeerAddr>,
    keep: impl Fn(&PeerAddr) -> bool,
) -> (Vec<(PeerAddr, Ask)>, usize) {
    let mut new = HashSet::new();
    let mut fresh: Vec<PeerAddr> =
        gossip.iter().copied().filter(|a| keep(a) && !seen.contains(a) && new.insert(*a)).collect();
    let found = fresh.len();
    fresh.shuffle(&mut rand::rng());
    fresh.truncate(MAX_UNPROVEN_PER_CRAWL);
    (fresh.into_iter().map(|a| (a, Ask::default())).collect(), found)
}

/// Probes every target over its network's route, each network with its own concurrency limit.
/// With a `deadline`, no new probe starts after it (running ones finish within their timeouts);
/// probes that didn't start aren't recorded and keep their place for the next crawl.
async fn probe_all(
    ctx: &NetCtx,
    routes: &Routes,
    targets: Vec<(PeerAddr, Ask)>,
    tip: i32,
    deadline: Option<Instant>,
) -> Vec<Outcome> {
    let net = ctx.net;
    let mut by_net: [Vec<(PeerAddr, Ask)>; AddrNet::COUNT] = Default::default();
    for t in targets {
        by_net[t.0.net().index()].push(t);
    }
    let runs = AddrNet::ALL.into_iter().zip(by_net).filter(|(_, list)| !list.is_empty()).map(|(an, list)| {
        let route = routes.get(an).route.clone();
        let limits = ctx.crawl_cfg.limits(an);
        async move {
            let Some(route) = route else { return Vec::new() };
            let total = list.len();
            let route = &route;
            let done: Vec<Outcome> = futures::stream::iter(list)
                .take_while(|_| std::future::ready(deadline.is_none_or(|d| Instant::now() < d)))
                .map(|(addr, ask)| async move {
                    let result =
                        p2p::probe(net, &addr, route, tip, ask.getaddr, ask.announce.as_ref(), &limits.timeouts).await;
                    Outcome { addr, asked_addrs: ask.getaddr, result }
                })
                .buffer_unordered(limits.concurrency)
                .collect()
                .await;
            if done.len() < total {
                tracing::info!(
                    net = net.name(),
                    "{an}: out of time, {} of {total} probes left for the next crawl",
                    total - done.len()
                );
            }
            done
        }
    });
    futures::future::join_all(runs).await.into_iter().flatten().collect()
}

/// Pulls dashd's address manager into `nodes`; on an empty database, falls back to DNS seeds.
async fn learn_from_dashd(ctx: &NetCtx) -> Result<()> {
    let mut addrs = Vec::new();
    // Every network dashd knows (it only keeps onion/I2P/CJDNS addresses when it can reach them).
    match ctx.rpc.call::<Vec<Value>>("getnodeaddresses", json!([0])).await {
        Ok(list) => {
            for e in list {
                let (Some(host), Some(port)) = (e["address"].as_str(), e["port"].as_u64()) else {
                    continue;
                };
                if let Some(a) = u16::try_from(port).ok().and_then(|port| PeerAddr::from_parts(host, port)) {
                    addrs.push(a);
                }
            }
        }
        Err(e) => tracing::warn!(net = ctx.net.name(), "getnodeaddresses: {e:#}"),
    }
    addrs.retain(|a| is_trusted_gossip(ctx.net, a));
    let empty =
        ctx.db.read(|c| Ok(c.query_row("SELECT count(*) FROM nodes", [], |r| r.get::<_, i64>(0))? == 0)).await?;
    if addrs.is_empty() && empty {
        for seed in ctx.net.dns_seeds() {
            match tokio::net::lookup_host((*seed, ctx.net.default_port())).await {
                Ok(found) => addrs.extend(found.map(PeerAddr::from_socket)),
                Err(e) => tracing::warn!("dns seed {seed}: {e}"),
            }
        }
        upsert_addrs(ctx, addrs, "seed").await
    } else {
        upsert_addrs(ctx, addrs, "dashd").await
    }
}

async fn upsert_addrs(ctx: &NetCtx, addrs: Vec<PeerAddr>, source: &'static str) -> Result<()> {
    if addrs.is_empty() {
        return Ok(());
    }
    ctx.db
        .write(move |c| {
            let now = db::now();
            let tx = c.transaction()?;
            {
                let mut st = tx.prepare_cached(
                    "INSERT INTO nodes (addr, network, source, first_seen, last_gossip) VALUES (?1, ?2, ?3, ?4, ?4)
                     ON CONFLICT(addr) DO UPDATE SET last_gossip = excluded.last_gossip",
                )?;
                for a in &addrs {
                    st.execute(params![a.to_string(), a.net().name(), source, now])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
}

struct ProbeRow {
    addr: String,
    net: AddrNet,
    asked_addrs: bool,
    announced: bool,
    ok: Option<p2p::Version>,
    mnauth: Option<MnAuth>,
    error: Option<String>,
    geo: Option<crate::geo::GeoInfo>,
}

impl ProbeRow {
    fn from(o: Outcome, geo: &crate::geo::Geo) -> ProbeRow {
        // Onion, I2P and CJDNS addresses say nothing about where a node is.
        let info = o.addr.public_ip().filter(|_| geo.loaded()).map(|ip| geo.lookup(ip));
        let (ok, mnauth, announced, error) = match o.result {
            Ok(p) => (Some(p.version), p.mnauth, p.announced, None),
            Err(e) => (None, None, false, Some(format!("{e:#}"))),
        };
        ProbeRow {
            addr: o.addr.to_string(),
            net: o.addr.net(),
            asked_addrs: o.asked_addrs,
            announced,
            ok,
            mnauth,
            error,
            geo: info,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn record(
    c: &mut Connection,
    id: i64,
    now: i64,
    tip: i64,
    tried: usize,
    discovered: usize,
    rows: Vec<ProbeRow>,
    routes: &Routes,
) -> Result<()> {
    let mn_protx = proven_masternodes(c, &rows)?;
    let tx = c.transaction()?;
    {
        let mut ensure = tx.prepare_cached(
            "INSERT INTO nodes (addr, network, source, first_seen) VALUES (?1, ?2, 'mnlist', ?3)
             ON CONFLICT(addr) DO NOTHING",
        )?;
        let mut ok_st = tx.prepare_cached(
            "UPDATE nodes SET last_tried = ?2, last_success = ?2, fail_streak = 0, next_try = ?2,
                last_crawl = ?3, last_ok = 1, last_error = NULL, user_agent = ?4, protocol = ?5, services = ?6,
                start_height = ?7, latency_ms = ?8, last_getaddr = CASE WHEN ?9 THEN ?2 ELSE last_getaddr END,
                relay = ?10, mn_protx = ?11, last_announce = CASE WHEN ?12 THEN ?2 ELSE last_announce END
             WHERE addr = ?1",
        )?;
        // Failures: nodes seen recently are retried every crawl (they count toward uptime);
        // never-reached addresses back off 15 min, 30 min, ... up to a day, then weekly.
        let mut fail_st = tx.prepare_cached(
            "UPDATE nodes SET last_tried = ?2, fail_streak = fail_streak + 1, last_crawl = ?3, last_ok = 0,
                last_error = ?4,
                next_try = CASE
                    WHEN last_success > ?2 - ?5 THEN ?2
                    WHEN last_success IS NULL AND fail_streak + 1 >= ?6 THEN ?2 + 7 * 86400
                    ELSE ?2 + min(900 * (1 << min(fail_streak, 7)), 86400)
                END
             WHERE addr = ?1",
        )?;
        // Geo columns keep their old values when the databases aren't loaded (first start, or a
        // failed download), so a crawl never blanks them.
        let mut geo_st = tx.prepare_cached(
            "UPDATE nodes SET country = ?2, continent = ?3, city = ?4, lat = ?5, lon = ?6, asn = ?7, as_org = ?8
             WHERE addr = ?1",
        )?;
        for (r, mn_protx) in rows.iter().zip(mn_protx) {
            ensure.execute(params![r.addr, r.net.name(), now])?;
            match &r.ok {
                Some(v) => ok_st.execute(params![
                    r.addr,
                    now,
                    id,
                    v.user_agent,
                    v.protocol,
                    v.services as i64,
                    v.start_height,
                    v.latency_ms,
                    r.asked_addrs,
                    v.relay,
                    mn_protx,
                    r.announced,
                ])?,
                None => fail_st.execute(params![r.addr, now, id, r.error, ACTIVE_WINDOW, NEW_ADDR_TRIES])?,
            };
            if let Some(g) = &r.geo {
                geo_st.execute(params![r.addr, g.country, g.continent, g.city, g.lat, g.lon, g.asn, g.as_org])?;
            }
        }
    }
    tx.execute(
        "INSERT INTO mn_daily (protx, day, attempts, ok)
         SELECT m.protx, ?2, 1, n.last_ok FROM masternodes m JOIN nodes n ON n.addr = m.service
         WHERE m.removed IS NULL AND n.last_crawl = ?1
         ON CONFLICT(protx, day) DO UPDATE SET attempts = attempts + 1, ok = ok + excluded.ok",
        params![id, day_of(now)],
    )?;
    tx.execute(
        "INSERT INTO mn_versions (protx, ts, user_agent, protocol)
         SELECT m.protx, ?2, n.user_agent, n.protocol FROM masternodes m JOIN nodes n ON n.addr = m.service
         WHERE m.removed IS NULL AND n.last_crawl = ?1 AND n.last_ok = 1
           AND NOT EXISTS (
             SELECT 1 FROM (SELECT user_agent, protocol FROM mn_versions WHERE protx = m.protx ORDER BY ts DESC LIMIT 1) v
             WHERE v.user_agent = n.user_agent AND v.protocol = n.protocol)",
        params![id, now],
    )?;

    let agg = aggregate(&tx, id, now)?;
    let networks = network_stats(&tx, id, now, Some(routes))?;
    {
        let mut ins =
            tx.prepare_cached("INSERT INTO crawl_agg (crawl_id, scope, dim, key, count) VALUES (?1, ?2, ?3, ?4, ?5)")?;
        for ((scope, dim, key), count) in &agg.counts.0 {
            ins.execute(params![id, scope, dim, key, count])?;
        }
    }
    tx.execute(
        "UPDATE crawls SET finished = ?2, tip_height = ?3, tried = ?4, reachable = ?5, discovered = ?6,
            mn_total = ?7, mn_reachable = ?8, networks = ?9 WHERE id = ?1",
        params![
            id,
            db::now(),
            tip,
            tried,
            agg.reachable,
            discovered,
            agg.mn_total,
            agg.mn_reachable,
            Value::from(networks).to_string()
        ],
    )?;
    tx.commit()?;
    Ok(())
}

/// The masternode each row that answered proved to be by MNAUTH: signed with a registered
/// masternode's operator key, on whatever network we reached it. An address the masternode's ProTx
/// lists needs no signature check, which spares nearly every one. Done before the crawl's write
/// transaction: a BLS check takes about a millisecond.
fn proven_masternodes(c: &Connection, rows: &[ProbeRow]) -> Result<Vec<Option<String>>> {
    let keys: HashMap<String, String> = query_all(
        c,
        "SELECT protx, pubkey_operator FROM masternodes WHERE removed IS NULL AND pubkey_operator IS NOT NULL",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let listed: HashSet<(String, String)> =
        query_all(c, "SELECT protx, addr FROM mn_addrs", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let mut forged = 0;
    let proven = rows
        .iter()
        .map(|r| {
            let (Some(v), Some(a)) = (&r.ok, &r.mnauth) else { return None };
            let ok = listed.contains(&(a.pro_tx_hash.clone(), r.addr.clone()))
                || keys.get(&a.pro_tx_hash).is_some_and(|k| a.verify(v.protocol, k));
            forged += usize::from(!ok);
            ok.then(|| a.pro_tx_hash.clone())
        })
        .collect();
    if forged > 0 {
        tracing::debug!("{forged} MNAUTH replies not signed by a registered masternode");
    }
    Ok(proven)
}

/// Node counts keyed by (scope, dim, key).
#[derive(Default)]
struct Counts(HashMap<(&'static str, &'static str, String), i64>);

impl Counts {
    fn add(&mut self, scope: &'static str, dim: &'static str, key: String) {
        self.add_n(scope, dim, key, 1);
    }

    /// Zero counts are left out, so an empty bucket produces no row.
    fn add_n(&mut self, scope: &'static str, dim: &'static str, key: String, n: i64) {
        if n > 0 {
            *self.0.entry((scope, dim, key)).or_default() += n;
        }
    }
}

struct Aggregate {
    counts: Counts,
    reachable: i64,
    mn_total: i64,
    mn_reachable: i64,
}

fn asn_key(asn: Option<i64>, org: Option<String>) -> String {
    match asn {
        Some(n) => format!("AS{n} {}", org.unwrap_or_default()).trim_end().to_string(),
        None => "Unknown".into(),
    }
}

/// Distribution counts for this crawl. Scopes:
/// - `all`: every node that answered this crawl; `nonmn`: those that aren't a registered masternode
///   (by the addresses its ProTx lists, or by MNAUTH on any other address).
/// - `mn` / `evo`: every enabled (not PoSe-banned) masternode / evonode, reachable or not, with the
///   user agent it last announced within the active window.
///
/// Each address counts as a node: a node reachable over both IPv4 and Tor counts twice.
fn aggregate(c: &Connection, id: i64, now: i64) -> Result<Aggregate> {
    let mut counts = Counts::default();

    let mn_addrs: HashSet<String> = query_all(c, "SELECT addr FROM mn_addrs", [], |r| r.get(0))?;

    let mut reachable = 0;
    let mut st = c.prepare(
        "SELECT addr, user_agent, protocol, country, asn, as_org, network, mn_protx IS NOT NULL
         FROM nodes WHERE last_crawl = ?1 AND last_ok = 1",
    )?;
    let mut rows = st.query([id])?;
    while let Some(r) = rows.next()? {
        reachable += 1;
        let addr: String = r.get(0)?;
        let ua: String = r.get::<_, Option<String>>(1)?.unwrap_or_default();
        let protocol: i64 = r.get::<_, Option<i64>>(2)?.unwrap_or(0);
        let country = r.get::<_, Option<String>>(3)?.unwrap_or_else(|| "Unknown".into());
        let asn = asn_key(r.get(4)?, r.get(5)?);
        let network = r.get::<_, Option<String>>(6)?.unwrap_or_else(|| "Unknown".into());
        let is_mn = r.get::<_, bool>(7)? || mn_addrs.contains(&addr);
        let scopes: &[&'static str] = if is_mn { &["all"] } else { &["all", "nonmn"] };
        for &s in scopes {
            counts.add(s, "ua", ua.clone());
            counts.add(s, "version", ua::version_label(&ua));
            counts.add(s, "client", if ua::is_dash_core(&ua) { "Dash Core".into() } else { "Other".into() });
            counts.add(s, "protocol", protocol.to_string());
            counts.add(s, "country", country.clone());
            counts.add(s, "asn", asn.clone());
            counts.add(s, "network", network.clone());
        }
    }
    // Network-wide reachability over the active set: recently seen nodes plus enabled masternodes
    // that failed this crawl. Every such address on a crawled network is probed every crawl, so
    // this only leaves out networks we can't reach and proxied probes that ran out of time:
    // known, but not crawled, isn't unreachable.
    let unreachable: i64 = c.query_row(
        "SELECT count(*) FROM nodes n WHERE n.last_crawl = ?1 AND n.last_ok = 0
           AND (n.last_success > ?2 OR n.addr IN
                (SELECT service FROM masternodes WHERE removed IS NULL AND coalesce(pose_ban_height, -1) = -1))",
        params![id, now - ACTIVE_WINDOW],
        |r| r.get(0),
    )?;
    counts.add_n("all", "reachable", "yes".into(), reachable);
    counts.add_n("all", "reachable", "no".into(), unreachable);

    let (mut mn_total, mut mn_reachable) = (0, 0);
    let mut st = c.prepare(
        "SELECT m.type, coalesce(m.pose_ban_height, -1), n.last_crawl, n.last_ok, n.last_success, n.user_agent,
                n.protocol, n.country, n.asn, n.as_org, m.service
         FROM masternodes m LEFT JOIN nodes n ON n.addr = m.service WHERE m.removed IS NULL",
    )?;
    let mut rows = st.query([])?;
    while let Some(r) = rows.next()? {
        let mn_type: String = r.get(0)?;
        let status = pose_status(r.get(1)?);
        // Network of the primary service address.
        let network = r.get::<_, Option<String>>(10)?.as_deref().and_then(net_of).map(AddrNet::name);
        let evo = mn_type == "Evo";
        counts.add("mn", "status", status.into());
        if evo {
            counts.add("evo", "status", status.into());
        }
        if status != "enabled" {
            continue;
        }
        mn_total += 1;
        let checked = r.get::<_, Option<i64>>(2)? == Some(id);
        let ok = checked && r.get::<_, Option<i64>>(3)? == Some(1);
        mn_reachable += i64::from(ok);
        // Not checked: no usable service address (e.g. IPv6 without an IPv6 route).
        let reach = if ok {
            "yes"
        } else if checked {
            "no"
        } else {
            "unchecked"
        };
        let recent = r.get::<_, Option<i64>>(4)?.is_some_and(|t| now - t < ACTIVE_WINDOW);
        let ua = if recent { r.get::<_, Option<String>>(5)?.unwrap_or_default() } else { "Unknown".into() };
        let version = if recent { ua::version_label(&ua) } else { "Unknown".into() };
        let protocol = if recent { r.get::<_, Option<i64>>(6)?.map(|p| p.to_string()) } else { None };
        let country = r.get::<_, Option<String>>(7)?.unwrap_or_else(|| "Unknown".into());
        let asn = asn_key(r.get(8)?, r.get(9)?);
        let scopes: &[&'static str] = if evo { &["mn", "evo"] } else { &["mn"] };
        for &s in scopes {
            counts.add(s, "type", mn_type.clone());
            counts.add(s, "reachable", reach.into());
            counts.add(s, "ua", ua.clone());
            counts.add(s, "version", version.clone());
            counts.add(s, "protocol", protocol.clone().unwrap_or_else(|| "Unknown".into()));
            counts.add(s, "country", country.clone());
            counts.add(s, "asn", asn.clone());
            counts.add(s, "network", network.unwrap_or("Unknown").into());
        }
    }
    Ok(Aggregate { counts, reachable, mn_total, mn_reachable })
}

/// Per address network, for a crawl (the `networks` array of /summary, always all five, in
/// `AddrNet::ALL` order):
/// - `known`: addresses gossiped, listed by dashd or the masternode list, or reachable within the
///   active window before `now`, or tried in this crawl;
/// - `tried` / `reachable`: probed / answered in crawl `id`;
/// - `masternodes`: distinct registered masternodes reachable on the network, at an address their
///   ProTx lists or one that proved to be them by MNAUTH. Masternodes, not addresses: MNAUTH can be
///   relayed, so one masternode may answer at any number of addresses;
/// - `crawled` / `reason`: whether the crawl had a route to the network, and why not. Without
///   `routes` (crawls recorded before this was stored), a network counts as crawled if anything
///   on it was tried.
pub fn network_stats(c: &Connection, id: i64, now: i64, routes: Option<&Routes>) -> Result<Vec<Value>> {
    let mut by_net: HashMap<String, [i64; 4]> = HashMap::new();
    let mut st = c.prepare(
        "SELECT n.network,
                sum(max(coalesce(n.last_gossip, 0), coalesce(n.last_success, 0)) > ?2 OR n.last_crawl = ?1),
                sum(n.last_crawl = ?1),
                sum(n.last_crawl = ?1 AND n.last_ok = 1),
                count(DISTINCT CASE WHEN n.last_crawl = ?1 AND n.last_ok = 1 THEN coalesce(n.mn_protx, m.protx) END)
         FROM nodes n LEFT JOIN (SELECT addr, min(protx) AS protx FROM mn_addrs GROUP BY addr) m ON m.addr = n.addr
         WHERE n.network IS NOT NULL GROUP BY n.network",
    )?;
    let mut rows = st.query(params![id, now - ACTIVE_WINDOW])?;
    while let Some(r) = rows.next()? {
        let mut v = [0i64; 4];
        for (i, x) in v.iter_mut().enumerate() {
            *x = r.get::<_, Option<i64>>(i + 1)?.unwrap_or(0);
        }
        by_net.insert(r.get(0)?, v);
    }
    Ok(AddrNet::ALL
        .into_iter()
        .map(|n| {
            let [known, tried, reachable, masternodes] = by_net.get(n.name()).copied().unwrap_or_default();
            let (crawled, reason) = match routes {
                Some(r) => (r.get(n).route.is_some(), r.get(n).reason.clone()),
                None => (tried > 0, None),
            };
            json!({
                "network": n.name(), "known": known, "tried": tried, "reachable": reachable,
                "masternodes": masternodes, "crawled": crawled, "reason": reason,
            })
        })
        .collect())
}

/// Daily rollups for finished days, then retention.
pub fn maintenance(c: &mut Connection) -> Result<()> {
    let now = db::now();
    let today_start = now - now.rem_euclid(DAY);
    let first: Option<i64> = c.query_row("SELECT min(started) FROM crawls", [], |r| r.get(0)).optional()?.flatten();
    let Some(first) = first else { return Ok(()) };
    let mut day_start = match get_meta(c, "rollup_through")? {
        Some(d) => d.parse::<i64>()? + DAY,
        None => first - first.rem_euclid(DAY),
    };
    while day_start < today_start {
        let tx = c.transaction()?;
        let crawls: i64 = tx.query_row(
            "SELECT count(*) FROM crawls WHERE finished IS NOT NULL AND started >= ?1 AND started < ?2",
            params![day_start, day_start + DAY],
            |r| r.get(0),
        )?;
        if crawls > 0 {
            tx.execute(
                "INSERT OR REPLACE INTO daily_agg (day, scope, dim, key, avg)
                 SELECT ?3, scope, dim, key, CAST(sum(count) AS REAL) / ?4 FROM crawl_agg
                 WHERE crawl_id IN (SELECT id FROM crawls WHERE finished IS NOT NULL AND started >= ?1 AND started < ?2)
                 GROUP BY scope, dim, key",
                params![day_start, day_start + DAY, day_of(day_start), crawls],
            )?;
        }
        set_meta(&tx, "rollup_through", &day_start.to_string())?;
        tx.commit()?;
        day_start += DAY;
    }
    c.execute(
        "DELETE FROM crawl_agg WHERE crawl_id IN (SELECT id FROM crawls WHERE started < ?1)",
        [now - PER_CRAWL_RETENTION],
    )?;
    c.execute("DELETE FROM mn_daily WHERE day < ?1", [day_of(now - MN_DAILY_RETENTION)])?;
    c.execute(
        "DELETE FROM nodes WHERE first_seen < ?1 AND coalesce(last_success, 0) < ?1 AND coalesce(last_gossip, 0) < ?1
           AND addr NOT IN (SELECT service FROM masternodes WHERE service IS NOT NULL)
           AND addr NOT IN (SELECT addr FROM mn_addrs)",
        [now - NODE_RETENTION],
    )?;
    // Unfinished crawls are left over from a restart mid-crawl.
    c.execute("DELETE FROM crawls WHERE finished IS NULL AND started < ?1", [now - DAY])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mnauth::vectors::{VECTORS, auth};
    use crate::transport::{Toggle, TransportConfig};

    fn reached(addr: &str, protocol: i32, mnauth: Option<MnAuth>) -> ProbeRow {
        let addr: PeerAddr = addr.parse().unwrap();
        let version = p2p::Version {
            protocol,
            services: 1,
            user_agent: "/Dash Core:24.0.0/".into(),
            start_height: 1,
            relay: true,
            latency_ms: 1,
        };
        ProbeRow {
            addr: addr.to_string(),
            net: addr.net(),
            asked_addrs: false,
            announced: false,
            ok: Some(version),
            mnauth,
            error: None,
            geo: None,
        }
    }

    /// dashd's getnetworkinfo, trimmed: what a testnet dashd with an onion service and an I2P
    /// destination reports.
    fn networkinfo() -> Value {
        json!({
            "localservices": "0000000000000c04",
            "localaddresses": [
                {"address": "ntn5iqxkkvhgvhiax3wpvsapvwdrnxyosujbrfyi2fltrbeygfg52gqd.onion", "port": 19999, "score": 4},
                {"address": "tzjs7fa7xnfnukeauhgpvol5eieojj2luhdn5skyuirnzzuojm6q.b32.i2p", "port": 0, "score": 4},
                {"address": "203.0.113.7", "port": 19999, "score": 1},
                {"address": "pg6mmjiyjmcrsslvykfwnntlaru7p5svn6y2ymmju6nubxndf4pscryd.onion", "port": 9999, "score": 1},
            ],
        })
    }

    #[test]
    fn beacon_is_our_onion_and_i2p_addresses() {
        let b = beacon_from(Network::Testnet, &networkinfo());
        let addrs: Vec<String> = b.iter().map(|a| a.addr.to_string()).collect();
        // Clearnet isn't ours to announce (and is private here), and nothing off the default port.
        assert_eq!(
            addrs,
            [
                "ntn5iqxkkvhgvhiax3wpvsapvwdrnxyosujbrfyi2fltrbeygfg52gqd.onion:19999",
                "tzjs7fa7xnfnukeauhgpvol5eieojj2luhdn5skyuirnzzuojm6q.b32.i2p:0"
            ]
        );
        assert!(b.iter().all(|a| a.services == 0xc04));
        // A dashd that doesn't accept inbound has none, and a reply without services gives nothing.
        assert!(beacon_from(Network::Testnet, &json!({"localservices": "0c04", "localaddresses": []})).is_empty());
        let mut info = networkinfo();
        info.as_object_mut().unwrap().remove("localservices");
        assert!(beacon_from(Network::Testnet, &info).is_empty());
    }

    #[test]
    fn announcements_go_out_daily_to_each_node() {
        let beacon = beacon_from(Network::Testnet, &networkinfo());
        let mut rng = rand::rngs::StdRng::seed_from_u64(1);
        let now = 10 * DAY;
        assert!(pick_announce(&beacon, None, now, &mut rng).is_some());
        assert!(pick_announce(&beacon, Some(now - ANNOUNCE_EVERY), now, &mut rng).is_some());
        assert!(pick_announce(&beacon, Some(now - ANNOUNCE_EVERY + 1), now, &mut rng).is_none());
        assert!(pick_announce(&[], None, now, &mut rng).is_none());
        // Both addresses get their turn.
        let picked: HashSet<PeerAddr> =
            (0..64).filter_map(|_| pick_announce(&beacon, None, now, &mut rng)).map(|a| a.addr).collect();
        assert_eq!(picked.len(), 2);
    }

    #[test]
    fn record_remembers_announcements() {
        let mut c = db::open_memory();
        let routes = TransportConfig {
            ipv6: Toggle::Off,
            ipv6_proxy: None,
            tor_proxy: None,
            i2p_proxy: None,
            cjdns: Toggle::Off,
        }
        .routes();
        let last = |c: &Connection, a: &str| -> Option<i64> {
            c.query_row("SELECT last_announce FROM nodes WHERE addr = ?1", [a], |r| r.get(0)).unwrap()
        };
        c.execute("INSERT INTO crawls (id, started) VALUES (1, 100)", []).unwrap();
        let told = ProbeRow { announced: true, ..reached("5.6.7.8:9999", 70242, None) };
        record(&mut c, 1, 100, 1, 2, 0, vec![told, reached("5.6.7.9:9999", 70242, None)], &routes).unwrap();
        assert_eq!(last(&c, "5.6.7.8:9999"), Some(100));
        assert_eq!(last(&c, "5.6.7.9:9999"), None);
        // A later probe that announces nothing keeps the time.
        c.execute("INSERT INTO crawls (id, started) VALUES (2, 200)", []).unwrap();
        record(&mut c, 2, 200, 1, 1, 0, vec![reached("5.6.7.8:9999", 70242, None)], &routes).unwrap();
        assert_eq!(last(&c, "5.6.7.8:9999"), Some(100));
    }

    /// An onion address that signs our challenge with a masternode's operator key counts as that
    /// masternode; one that names a masternode whose key didn't sign it doesn't.
    #[test]
    fn mnauth_makes_an_onion_address_a_masternode() {
        let mut c = db::open_memory();
        let (mn, other) = (&VECTORS[0], &VECTORS[1]);
        for (v, service) in [(mn, "1.2.3.4:9999"), (other, "1.2.3.5:9999")] {
            c.execute(
                "INSERT INTO masternodes (protx, type, service, pubkey_operator, first_seen, updated)
                 VALUES (?1, 'Regular', ?2, ?3, 0, 0)",
                params![v.protx, service, v.pubkey],
            )
            .unwrap();
        }
        c.execute("INSERT INTO crawls (id, started) VALUES (1, 0)", []).unwrap();
        let proven = "pg6mmjiyjmcrsslvykfwnntlaru7p5svn6y2ymmju6nubxndf4pscryd.onion:9999";
        let forged = "5v5lgddolcidtt2qmhmvyka2ewht4mkmmj73tfwuimlckgmqb5lthtid.onion:9999";
        // A real signature by `mn`, presented as `other`'s.
        let claim = MnAuth { pro_tx_hash: other.protx.into(), ..auth(mn) };
        // MNAUTH can be relayed: another address answering for the same masternode doesn't make two.
        let relay = "a4hwvpuckrcdssqamovhwmh4arqqi3nrskhs6eu2ojdsefspm7q7wzad.onion:9999";
        let rows = vec![
            reached(proven, mn.protocol, Some(auth(mn))),
            reached(relay, mn.protocol, Some(auth(mn))),
            reached(forged, mn.protocol, Some(claim)),
            reached("5.6.7.8:9999", 70242, None),
            // At the address its ProTx lists, the claim isn't checked: the ProTx already ties them.
            reached("1.2.3.4:9999", mn.protocol, Some(MnAuth { sig: [0; 96], ..auth(mn) })),
        ];
        let routes = TransportConfig {
            ipv6: Toggle::Off,
            ipv6_proxy: None,
            tor_proxy: Some("127.0.0.1:9050".into()),
            i2p_proxy: None,
            cjdns: Toggle::Off,
        }
        .routes();
        record(&mut c, 1, 100, 1, 3, 0, rows, &routes).unwrap();

        let protx_of = |c: &Connection, a: &str| -> Option<String> {
            c.query_row("SELECT mn_protx FROM nodes WHERE addr = ?1", [a], |r| r.get(0)).unwrap()
        };
        assert_eq!(protx_of(&c, proven).as_deref(), Some(mn.protx));
        assert_eq!(protx_of(&c, forged), None);
        assert_eq!(protx_of(&c, "5.6.7.8:9999"), None);
        assert_eq!(protx_of(&c, "1.2.3.4:9999").as_deref(), Some(mn.protx));

        let nets = network_stats(&c, 1, 100, Some(&routes)).unwrap();
        let onion = nets.iter().find(|n| n["network"] == "onion").unwrap();
        assert_eq!((onion["reachable"].as_i64(), onion["masternodes"].as_i64()), (Some(3), Some(1)));
        let ipv4 = nets.iter().find(|n| n["network"] == "ipv4").unwrap();
        assert_eq!((ipv4["reachable"].as_i64(), ipv4["masternodes"].as_i64()), (Some(2), Some(1)));
        let count = |scope: &str, key: &str| -> Option<i64> {
            c.query_row(
                "SELECT count FROM crawl_agg WHERE crawl_id = 1 AND scope = ?1 AND dim = 'network' AND key = ?2",
                [scope, key],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
        };
        assert_eq!(count("all", "onion"), Some(3));
        assert_eq!(count("nonmn", "onion"), Some(1));

        // Answering later without MNAUTH clears it.
        c.execute("INSERT INTO crawls (id, started) VALUES (2, 200)", []).unwrap();
        record(&mut c, 2, 200, 1, 1, 0, vec![reached(proven, mn.protocol, None)], &routes).unwrap();
        assert_eq!(protx_of(&c, proven), None);
    }
}

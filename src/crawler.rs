//! Periodic crawl of the P2P network: handshake every known address, ask some peers for more,
//! record what each node announces, then snapshot the distributions for the charts.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::StreamExt;
use rand::SeedableRng;
use rand::seq::SliceRandom;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::collector::pose_status;
use crate::db::{self, day_of, get_meta, query_all, set_meta};
use crate::p2p::{self, Probe};
use crate::state::NetCtx;
use crate::ua;

const DAY: i64 = 86_400;
/// A node that answered within this window still counts as part of the network when it misses a crawl.
const ACTIVE_WINDOW: i64 = 7 * DAY;
const GETADDR_EVERY: i64 = 6 * 3600;
/// Addresses that never answered get this many tries (with backoff) before going to weekly retries.
const NEW_ADDR_TRIES: i64 = 6;
/// Never-reached addresses probed per crawl, per pass. The rest wait for the next crawl.
const MAX_UNPROVEN_PER_CRAWL: usize = 5_000;
/// The first crawl asks this many peers for addresses, to bootstrap the address table.
const BOOTSTRAP_GETADDR: usize = 1_000;
const PER_CRAWL_RETENTION: i64 = 14 * DAY;
const MN_DAILY_RETENTION: i64 = 180 * DAY;
const NODE_RETENTION: i64 = 30 * DAY;

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

struct Outcome {
    addr: SocketAddr,
    asked_addrs: bool,
    result: Result<Probe>,
}

async fn crawl_once(ctx: &NetCtx) -> Result<i64> {
    let now = db::now();
    learn_from_dashd(ctx).await?;

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
            let mn: Vec<String> = query_all(
                c,
                "SELECT DISTINCT service FROM masternodes WHERE removed IS NULL AND service IS NOT NULL",
                [],
                |r| r.get(0),
            )?;
            let due: Vec<(String, Option<i64>, Option<i64>)> =
                query_all(c, "SELECT addr, last_success, last_getaddr FROM nodes WHERE next_try <= ?1", [now], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?;
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
    let usable = |a: &SocketAddr| ctx.crawl_cfg.usable(a);
    let mut targets: HashMap<SocketAddr, bool> = HashMap::new();
    for a in mn_addrs.iter().filter_map(|s| s.parse().ok()).filter(usable) {
        targets.insert(a, false);
    }
    // Ask nodes that answered recently for addresses, a random sample each crawl.
    let mut getaddr_candidates = Vec::new();
    let mut unproven = Vec::new();
    for (addr, last_success, last_getaddr) in &due {
        let Ok(a) = addr.parse::<SocketAddr>() else {
            continue;
        };
        if !usable(&a) {
            continue;
        }
        match last_success {
            Some(t) if now - t < ACTIVE_WINDOW => {
                targets.entry(a).or_insert(false);
                if last_getaddr.is_none_or(|t| now - t > GETADDR_EVERY) {
                    getaddr_candidates.push(a);
                }
            }
            _ => unproven.push(a),
        }
    }
    unproven.shuffle(&mut rng);
    for a in unproven.into_iter().take(MAX_UNPROVEN_PER_CRAWL) {
        targets.entry(a).or_insert(false);
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
        targets.insert(a, true);
    }

    let tip = ctx.status.read().unwrap().blocks as i32;
    let mut outcomes = probe_all(ctx, targets.iter().map(|(a, g)| (*a, *g)).collect(), tip).await;

    // Second pass: probe addresses we just learned about, so this crawl's picture is complete.
    // Gossip is only trusted for the network's default port: anything else could aim us at
    // arbitrary services on third-party hosts. Masternodes on other ports come from the list.
    let mut seen: HashSet<SocketAddr> = targets.keys().copied().collect();
    let mut gossip = Vec::new();
    for o in &mut outcomes {
        if let Ok(p) = &mut o.result {
            gossip.extend(
                std::mem::take(&mut p.addrs).into_iter().filter(|a| usable(a) && a.port() == ctx.net.default_port()),
            );
        }
    }
    let mut fresh: Vec<SocketAddr> = gossip.iter().copied().filter(|a| seen.insert(*a)).collect();
    let discovered = fresh.len();
    fresh.shuffle(&mut rng);
    fresh.truncate(MAX_UNPROVEN_PER_CRAWL);
    upsert_addrs(ctx, gossip, "getaddr").await?;
    outcomes.extend(probe_all(ctx, fresh.into_iter().map(|a| (a, false)).collect(), tip).await);

    let tried = outcomes.len();
    let rows: Vec<ProbeRow> = outcomes.into_iter().map(|o| ProbeRow::from(o, &ctx.geo)).collect();
    let tip = tip as i64;
    ctx.db.write(move |c| record(c, id, now, tip, tried, discovered, rows)).await.context("record crawl")?;
    Ok(id)
}

async fn probe_all(ctx: &NetCtx, targets: Vec<(SocketAddr, bool)>, tip: i32) -> Vec<Outcome> {
    let net = ctx.net;
    let timeouts = &ctx.crawl_cfg.timeouts;
    futures::stream::iter(targets)
        .map(|(addr, asked_addrs)| async move {
            let result = p2p::probe(net, addr, tip, asked_addrs, timeouts).await;
            Outcome { addr, asked_addrs, result }
        })
        .buffer_unordered(ctx.crawl_cfg.concurrency)
        .collect()
        .await
}

/// Pulls dashd's address manager into `nodes`; on an empty database, falls back to DNS seeds.
async fn learn_from_dashd(ctx: &NetCtx) -> Result<()> {
    let mut addrs = Vec::new();
    match ctx.rpc.call::<Vec<Value>>("getnodeaddresses", json!([0])).await {
        Ok(list) => {
            for e in list {
                let (Some(ip), Some(port)) = (e["address"].as_str(), e["port"].as_u64()) else {
                    continue;
                };
                let ip = ip.trim_matches(|c| c == '[' || c == ']');
                if let Ok(ip) = ip.parse() {
                    addrs.push(SocketAddr::new(ip, port as u16));
                }
            }
        }
        Err(e) => tracing::warn!(net = ctx.net.name(), "getnodeaddresses: {e:#}"),
    }
    addrs.retain(|a| ctx.crawl_cfg.usable(a) && a.port() == ctx.net.default_port());
    let empty =
        ctx.db.read(|c| Ok(c.query_row("SELECT count(*) FROM nodes", [], |r| r.get::<_, i64>(0))? == 0)).await?;
    if addrs.is_empty() && empty {
        for seed in ctx.net.dns_seeds() {
            match tokio::net::lookup_host((*seed, ctx.net.default_port())).await {
                Ok(found) => addrs.extend(found),
                Err(e) => tracing::warn!("dns seed {seed}: {e}"),
            }
        }
        upsert_addrs(ctx, addrs, "seed").await
    } else {
        upsert_addrs(ctx, addrs, "dashd").await
    }
}

async fn upsert_addrs(ctx: &NetCtx, addrs: Vec<SocketAddr>, source: &'static str) -> Result<()> {
    if addrs.is_empty() {
        return Ok(());
    }
    ctx.db
        .write(move |c| {
            let now = db::now();
            let tx = c.transaction()?;
            {
                let mut st = tx.prepare_cached(
                    "INSERT INTO nodes (addr, source, first_seen, last_gossip) VALUES (?1, ?2, ?3, ?3)
                     ON CONFLICT(addr) DO UPDATE SET last_gossip = excluded.last_gossip",
                )?;
                for a in &addrs {
                    st.execute(params![a.to_string(), source, now])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
}

struct ProbeRow {
    addr: String,
    asked_addrs: bool,
    ok: Option<p2p::Version>,
    error: Option<String>,
    geo: Option<crate::geo::GeoInfo>,
}

impl ProbeRow {
    fn from(o: Outcome, geo: &crate::geo::Geo) -> ProbeRow {
        let info = geo.loaded().then(|| geo.lookup(o.addr.ip()));
        let (ok, error) = match o.result {
            Ok(p) => (Some(p.version), None),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        ProbeRow { addr: o.addr.to_string(), asked_addrs: o.asked_addrs, ok, error, geo: info }
    }
}

fn record(
    c: &mut Connection,
    id: i64,
    now: i64,
    tip: i64,
    tried: usize,
    discovered: usize,
    rows: Vec<ProbeRow>,
) -> Result<()> {
    let tx = c.transaction()?;
    {
        let mut ensure = tx.prepare_cached(
            "INSERT INTO nodes (addr, source, first_seen) VALUES (?1, 'mnlist', ?2) ON CONFLICT(addr) DO NOTHING",
        )?;
        let mut ok_st = tx.prepare_cached(
            "UPDATE nodes SET last_tried = ?2, last_success = ?2, fail_streak = 0, next_try = ?2,
                last_crawl = ?3, last_ok = 1, last_error = NULL, user_agent = ?4, protocol = ?5, services = ?6,
                start_height = ?7, latency_ms = ?8, last_getaddr = CASE WHEN ?9 THEN ?2 ELSE last_getaddr END
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
        for r in &rows {
            ensure.execute(params![r.addr, now])?;
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
    {
        let mut ins =
            tx.prepare_cached("INSERT INTO crawl_agg (crawl_id, scope, dim, key, count) VALUES (?1, ?2, ?3, ?4, ?5)")?;
        for ((scope, dim, key), count) in &agg.counts.0 {
            ins.execute(params![id, scope, dim, key, count])?;
        }
    }
    tx.execute(
        "UPDATE crawls SET finished = ?2, tip_height = ?3, tried = ?4, reachable = ?5, discovered = ?6,
            mn_total = ?7, mn_reachable = ?8 WHERE id = ?1",
        params![id, db::now(), tip, tried, agg.reachable, discovered, agg.mn_total, agg.mn_reachable],
    )?;
    tx.commit()?;
    Ok(())
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
/// - `all`: every node that answered this crawl; `nonmn`: those that aren't a registered masternode.
/// - `mn` / `evo`: every enabled (not PoSe-banned) masternode / evonode, reachable or not, with the
///   user agent it last announced within the active window.
fn aggregate(c: &Connection, id: i64, now: i64) -> Result<Aggregate> {
    let mut counts = Counts::default();

    let mn_services: HashSet<String> =
        query_all(c, "SELECT service FROM masternodes WHERE removed IS NULL AND service IS NOT NULL", [], |r| {
            r.get(0)
        })?;

    let mut reachable = 0;
    let mut st = c.prepare(
        "SELECT addr, user_agent, protocol, country, asn, as_org FROM nodes WHERE last_crawl = ?1 AND last_ok = 1",
    )?;
    let mut rows = st.query([id])?;
    while let Some(r) = rows.next()? {
        reachable += 1;
        let addr: String = r.get(0)?;
        let ua: String = r.get::<_, Option<String>>(1)?.unwrap_or_default();
        let protocol: i64 = r.get::<_, Option<i64>>(2)?.unwrap_or(0);
        let country = r.get::<_, Option<String>>(3)?.unwrap_or_else(|| "Unknown".into());
        let asn = asn_key(r.get(4)?, r.get(5)?);
        let scopes: &[&'static str] = if mn_services.contains(&addr) { &["all"] } else { &["all", "nonmn"] };
        for &s in scopes {
            counts.add(s, "ua", ua.clone());
            counts.add(s, "version", ua::version_label(&ua));
            counts.add(s, "client", if ua::is_dash_core(&ua) { "Dash Core".into() } else { "Other".into() });
            counts.add(s, "protocol", protocol.to_string());
            counts.add(s, "country", country.clone());
            counts.add(s, "asn", asn.clone());
        }
    }
    // Network-wide reachability over the active set: recently seen nodes plus enabled masternodes.
    let unreachable: i64 = c.query_row(
        "SELECT count(*) FROM nodes n WHERE NOT (n.last_crawl = ?1 AND n.last_ok = 1)
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
                n.protocol, n.country, n.asn, n.as_org
         FROM masternodes m LEFT JOIN nodes n ON n.addr = m.service WHERE m.removed IS NULL",
    )?;
    let mut rows = st.query([])?;
    while let Some(r) = rows.next()? {
        let mn_type: String = r.get(0)?;
        let status = pose_status(r.get(1)?);
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
        // Not checked: no usable service address (e.g. IPv6 while IPv6 crawling is off).
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
        }
    }
    Ok(Aggregate { counts, reachable, mn_total, mn_reachable })
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
           AND addr NOT IN (SELECT service FROM masternodes WHERE service IS NOT NULL)",
        [now - NODE_RETENTION],
    )?;
    // Unfinished crawls are left over from a restart mid-crawl.
    c.execute("DELETE FROM crawls WHERE finished IS NULL AND started < ?1", [now - DAY])?;
    Ok(())
}

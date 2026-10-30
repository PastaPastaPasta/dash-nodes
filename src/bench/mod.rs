//! `dash-nodes bench`: masternode speed tests from one vantage point.
//!
//! Standalone (no dashd): it reads the masternode list and reference block hashes from SOURCE (the
//! app's API, or nodes.dashhq.org), tests enabled masternodes over their core P2P address (see
//! probe.rs), keeps results in STATE_DIR and publishes them to the Worker (see publish.rs).
//!
//! - default: run forever, one full pass over the network per CYCLE_DAYS (see schedule.rs), and
//!   publish changed documents every BENCH_PUBLISH_INTERVAL seconds. Idles until a token exists.
//! - `--sweep`: test every masternode without a result from the current cycle now, at
//!   BENCH_SWEEP_CONCURRENCY, then publish (if a token is set) and exit.
//! - `--once <addr>`: test one node and print the result.

mod grade;
mod probe;
mod publish;
mod schedule;
mod store;
mod wire;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args;
use serde::Deserialize;
use tokio::task::JoinSet;

use self::probe::{BenchResult, CheckedReference, Meter, Params};
use self::publish::{Uploader, Vantage};
use self::schedule::{RateWindow, due_at, interval, is_late, jittered, subnet_key, tested_since};
use self::store::{History, Store, remember};
use crate::TransportArgs;
use crate::addr::{AddrNet, PeerAddr};
use crate::network::Network;
use crate::reference::{self, Reference};
use crate::transport::{self, Route, Routes};

#[derive(Args)]
pub struct BenchArgs {
    /// Test one node (ip:port, [ipv6]:port, onion, I2P or host:port), print the result and exit.
    #[arg(long, value_name = "ADDR", conflicts_with = "sweep")]
    once: Option<String>,
    /// Network of the --once node.
    #[arg(long, default_value = "mainnet", requires = "once")]
    network: String,
    /// Test every masternode not tested this cycle now, publish, and exit.
    #[arg(long)]
    sweep: bool,
    /// With --sweep: test at most this many masternodes.
    #[arg(long, requires = "sweep")]
    limit: Option<usize>,
    /// This vantage point's id ([a-z0-9-]{2,16}); results are published under it.
    #[arg(long, env = "VANTAGE")]
    vantage: Option<String>,
    /// Where the vantage is, for people ("DigitalOcean AMS3, Amsterdam").
    #[arg(long, env = "VANTAGE_LOCATION", default_value = "")]
    vantage_location: String,
    /// Where to read masternode lists and reference hashes: the app, or the public site.
    #[arg(long, env = "SOURCE", default_value = "https://nodes.dashhq.org")]
    source: String,
    /// The Worker to publish to (its origin; documents go to {TARGET}/_ingest/<key>).
    #[arg(long, env = "TARGET")]
    target: Option<String>,
    #[arg(long, env = "BENCH_TOKEN", hide_env_values = true)]
    bench_token: Option<String>,
    /// File holding the ingest bearer token; re-read until it has one.
    #[arg(long, env = "BENCH_TOKEN_FILE")]
    bench_token_file: Option<PathBuf>,
    #[arg(long, env = "NETWORKS", default_value = "mainnet testnet")]
    networks: String,
    /// One full pass over the network per this many days.
    #[arg(long, env = "CYCLE_DAYS", default_value_t = 14.0)]
    cycle_days: f64,
    /// Tests running at once (paced mode).
    #[arg(long, env = "BENCH_CONCURRENCY", default_value_t = 2)]
    concurrency: usize,
    /// Tests running at once with --sweep.
    #[arg(long, env = "BENCH_SWEEP_CONCURRENCY", default_value_t = 8)]
    sweep_concurrency: usize,
    #[arg(long, env = "STATE_DIR", default_value = "./bench-state")]
    state_dir: PathBuf,
    /// Headers to download per test (a multiple of 2000, at most 50000).
    #[arg(long, env = "BENCH_DEPTH", default_value_t = 50_000)]
    depth: u32,
    /// Seconds without a headers batch before re-requesting (a stall).
    #[arg(long, env = "BENCH_STALL_SECS", default_value_t = 10)]
    stall_secs: u64,
    /// Seconds one test may take in total.
    #[arg(long, env = "BENCH_TEST_TIMEOUT", default_value_t = 120)]
    test_timeout: u64,
    #[arg(long, env = "BENCH_PINGS", default_value_t = 5)]
    pings: usize,
    /// Don't start tests while downloads from all running tests exceed this (megabits/s).
    #[arg(long, env = "BENCH_MAX_MBIT", default_value_t = 24.0)]
    max_mbit: f64,
    /// Seconds between publishing rounds.
    #[arg(long, env = "BENCH_PUBLISH_INTERVAL", default_value_t = 600)]
    publish_interval: u64,
    #[command(flatten)]
    transport: TransportArgs,
}

/// How often the masternode lists and reference hashes are re-read from SOURCE.
const REFRESH_EVERY: Duration = Duration::from_secs(600);
/// Everything is uploaded again this often, whether it changed or not.
const FULL_PUBLISH_EVERY: Duration = Duration::from_secs(24 * 3600);
/// How often the results are snapshotted to the Worker's state store.
const SNAPSHOT_EVERY: Duration = Duration::from_secs(3 * 3600);
/// Results older than this many cycles are dropped (masternodes that left the list).
const KEEP_CYCLES: i64 = 6;
/// The vantage check (see `Bench::online`): how many known-good masternodes it tries, how long each
/// may take, and how long tests wait (and an "offline" answer holds) after it fails.
const ONLINE_CHECK_NODES: usize = 3;
const ONLINE_CHECK_LIMIT: Duration = Duration::from_secs(15);
const OFFLINE_PAUSE: Duration = Duration::from_secs(60);

fn valid_vantage(v: &str) -> bool {
    (2..=16).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn cycle_secs(days: f64) -> i64 {
    (days * 86_400.0) as i64
}

pub async fn main(args: BenchArgs) -> Result<()> {
    if args.depth == 0 || !args.depth.is_multiple_of(reference::STEP) || args.depth > reference::DEPTH {
        bail!("BENCH_DEPTH must be a multiple of {} up to {}", reference::STEP, reference::DEPTH);
    }
    if args.cycle_days.is_nan() || args.cycle_days < 0.01 {
        bail!("CYCLE_DAYS must be at least 0.01");
    }
    if args.max_mbit.is_nan() || args.max_mbit <= 0.0 {
        bail!("BENCH_MAX_MBIT must be positive");
    }
    let vantage = match (&args.vantage, &args.once) {
        (Some(v), _) => v.clone(),
        (None, Some(_)) => "local".into(),
        (None, None) => bail!("set VANTAGE"),
    };
    if !valid_vantage(&vantage) {
        bail!("VANTAGE must match [a-z0-9-]{{2,16}}");
    }
    let params = Params {
        depth: args.depth,
        stall: Duration::from_secs(args.stall_secs.max(1)),
        limit: Duration::from_secs(args.test_timeout.max(30)),
        pings: args.pings,
    };
    let source = Source::new(&args.source);
    if let Some(addr) = &args.once {
        return once(&args, &source, addr, &params, &vantage).await;
    }
    let mut nets = Vec::new();
    for n in args.networks.split_whitespace() {
        nets.push(Network::parse(n).with_context(|| format!("unknown network {n}"))?);
    }
    let uploader = match &args.target {
        Some(t) => Some(Uploader::new(t, args.bench_token.clone(), args.bench_token_file.clone())),
        None if args.sweep => None,
        None => bail!("set TARGET (the Worker to publish to)"),
    };
    // Not configured yet: stay up quietly rather than crash-loop, and start once a token appears.
    if !args.sweep
        && let Some(u) = &uploader
        && u.token().is_none()
    {
        tracing::info!("no bench token yet (BENCH_TOKEN / BENCH_TOKEN_FILE); waiting for one");
        while u.token().is_none() {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }
    // Results live in the Worker's state store too (see snapshot.rs): a new host picks them up.
    let snapshots = match (&args.target, uploader.as_ref().and_then(Uploader::token)) {
        (Some(t), Some(token)) => Some(crate::snapshot::Remote::new(t, token)?),
        _ => None,
    };
    let snapshot_name = format!("bench-{vantage}");
    let db_path = args.state_dir.join("bench.sqlite");
    if let Some(r) = &snapshots
        && !db_path.exists()
    {
        std::fs::create_dir_all(&args.state_dir)?;
        r.restore(&snapshot_name, &db_path).await.with_context(|| format!("restore {snapshot_name}"))?;
    }
    let store = Store::open(&args.state_dir)?;
    let history = store.load()?;
    tracing::info!(
        "bench: vantage {vantage}, {} masternodes with results, cycle {} days",
        history.len(),
        args.cycle_days
    );
    Bench {
        store,
        history,
        nets: nets.into_iter().map(|n| (n, NetState::default())).collect(),
        source,
        uploader,
        snapshots: snapshots.map(|r| (r, snapshot_name, db_path)),
        params,
        vantage,
        meter: Meter::default(),
        cycle_secs: cycle_secs(args.cycle_days),
        online_check: HashMap::new(),
        last_clean: HashMap::new(),
        args,
    }
    .run()
    .await
}

/// Reads masternode lists and reference hashes from the app's API (or the Worker serving it).
struct Source {
    client: reqwest::Client,
    base: String,
}

#[derive(Deserialize)]
struct MnList {
    masternodes: Vec<MnEntry>,
}

#[derive(Deserialize)]
struct MnEntry {
    protx: String,
    status: String,
    service: Option<String>,
}

impl Source {
    fn new(base: &str) -> Source {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .user_agent(crate::p2p::user_agent())
            .build()
            .expect("http client");
        Source { client, base: base.trim_end_matches('/').to_owned() }
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}/api/v1/{path}", self.base);
        let r = self.client.get(&url).send().await.with_context(|| url.clone())?;
        if !r.status().is_success() {
            bail!("{url}: HTTP {}", r.status());
        }
        r.json().await.with_context(|| url.clone())
    }

    async fn reference(&self, net: Network) -> Result<CheckedReference> {
        CheckedReference::new(&self.get::<Reference>(&format!("{}/reference", net.name())).await?)
    }

    async fn masternodes(&self, net: Network) -> Result<Vec<MnEntry>> {
        Ok(self.get::<MnList>(&format!("{}/masternodes", net.name())).await?.masternodes)
    }
}

#[derive(Clone)]
struct Node {
    net: Network,
    protx: String,
    addr: PeerAddr,
    route: Route,
    subnet: String,
}

#[derive(Default)]
struct NetState {
    nodes: Vec<Node>,
    /// Registered masternodes (banned ones too, as in the SML), for the mnlistdiff check.
    registered: usize,
    reference: Option<Arc<CheckedReference>>,
}

struct Bench {
    args: BenchArgs,
    store: Store,
    history: History,
    nets: Vec<(Network, NetState)>,
    source: Source,
    uploader: Option<Uploader>,
    /// Where results are snapshotted: the store, the snapshot's name and the database file.
    snapshots: Option<(crate::snapshot::Remote, String, PathBuf)>,
    params: Params,
    vantage: String,
    meter: Meter,
    cycle_secs: i64,
    /// Per address network: the last vantage check (when, and whether it passed), and when a test
    /// last finished without an error.
    online_check: HashMap<AddrNet, (Instant, bool)>,
    last_clean: HashMap<AddrNet, Instant>,
}

/// Masternodes that can be tested: enabled, with a well-formed ProTx hash and a core P2P address
/// on a network we can reach.
fn testable(net: Network, list: &[MnEntry], routes: &Routes) -> Vec<Node> {
    list.iter()
        .filter(|m| m.status == "enabled" && m.protx.len() == 64 && m.protx.bytes().all(|b| b.is_ascii_hexdigit()))
        .filter_map(|m| {
            let addr: PeerAddr = m.service.as_deref()?.parse().ok()?;
            let route = routes.for_addr(&addr)?.clone();
            Some(Node { net, protx: m.protx.to_ascii_lowercase(), subnet: subnet_key(&addr), addr, route })
        })
        .collect()
}

impl Bench {
    async fn refresh(&mut self) {
        let routes = self.args.transport.config().routes();
        for (net, st) in &mut self.nets {
            match self.source.masternodes(*net).await {
                Ok(list) => {
                    st.registered = list.len();
                    st.nodes = testable(*net, &list, &routes);
                }
                Err(e) => tracing::warn!("{}: masternode list: {e:#}", net.name()),
            }
            match self.source.reference(*net).await {
                Ok(r) => st.reference = Some(Arc::new(r)),
                Err(e) => tracing::warn!("{}: reference: {e:#}", net.name()),
            }
        }
    }

    /// Nodes on networks that can be tested now (those with reference hashes), with their history.
    fn testable_now(&self) -> impl Iterator<Item = (&Node, &[BenchResult])> {
        self.nets.iter().filter(|(_, st)| st.reference.is_some()).flat_map(move |(net, st)| {
            st.nodes.iter().map(move |n| (n, self.history.get(&(*net, n.protx.clone())).map_or(&[][..], Vec::as_slice)))
        })
    }

    /// Pause between starts for the current targets (see schedule.rs).
    fn pace(&self) -> Duration {
        let now = crate::db::now();
        let (mut targets, mut failing, mut late) = (0, 0, false);
        for (_, h) in self.testable_now() {
            targets += 1;
            failing += usize::from(h.first().is_some_and(|r| !r.ok));
            late |= is_late(h, self.cycle_secs, now);
        }
        interval(self.cycle_secs, targets, failing, late)
    }

    /// The most overdue masternode that may start now: not running, its subnet free, and in a sweep
    /// not yet tested this run.
    fn pick(&self, now: i64, in_flight: &HashMap<tokio::task::Id, Node>, swept: &HashSet<String>) -> Option<Node> {
        let cycle_start = now - self.cycle_secs;
        let mut best: Option<(i64, &Node)> = None;
        for (n, history) in self.testable_now() {
            if swept.contains(&n.protx) || in_flight.values().any(|t| t.protx == n.protx || t.subnet == n.subnet) {
                continue;
            }
            let due = if self.args.sweep {
                if tested_since(history, cycle_start) {
                    continue;
                }
                0
            } else {
                due_at(history, self.cycle_secs)
            };
            if due <= now && best.is_none_or(|(d, b)| (due, &n.protx) < (d, &b.protx)) {
                best = Some((due, n));
            }
        }
        best.map(|(_, n)| n.clone())
    }

    fn state(&self, net: Network) -> &NetState {
        &self.nets.iter().find(|(n, _)| *n == net).expect("configured network").1
    }

    /// The test of `node`, ready to spawn.
    fn test(&self, node: Node) -> impl Future<Output = BenchResult> + use<> {
        let st = self.state(node.net);
        let reference = st.reference.clone().expect("picked from a network with a reference");
        let expected = Some(st.registered).filter(|&n| n > 0);
        let (params, vantage, meter) = (self.params, self.vantage.clone(), self.meter.clone());
        async move {
            let timeouts = transport::default_timeouts(node.addr.net());
            let target = probe::Target {
                net: node.net,
                addr: &node.addr,
                route: &node.route,
                timeouts: &timeouts,
                expected_mns: expected,
            };
            probe::run(&target, &reference, &params, &vantage, &meter).await
        }
    }

    async fn run(mut self) -> Result<()> {
        let sweep = self.args.sweep;
        let concurrency = if sweep { self.args.sweep_concurrency } else { self.args.concurrency }.max(1);
        let max_bps = self.args.max_mbit * 1e6;
        let mut tasks: JoinSet<BenchResult> = JoinSet::new();
        let mut in_flight: HashMap<tokio::task::Id, Node> = HashMap::new();
        // A sweep tests each masternode once, even if it fails.
        let mut swept: HashSet<String> = HashSet::new();
        let mut rate = RateWindow::new(Duration::from_secs(10));
        let mut next_refresh = Instant::now();
        let mut next_publish = Instant::now() + Duration::from_secs(60);
        // Per network: when everything was last uploaded regardless of changes.
        let mut full_publish: HashMap<Network, Instant> = HashMap::new();
        // The first one soon after a start, so a vantage that restarts often still gets saved.
        let mut next_snapshot = Instant::now() + Duration::from_secs(600);
        // Jittered first start, so restarts of several vantages don't line up.
        let mut next_start = if sweep { Instant::now() } else { Instant::now() + jittered(Duration::from_secs(30)) };
        let mut pace = Duration::ZERO;
        let mut started = 0usize;
        let mut last_status = Instant::now();
        loop {
            let now = Instant::now();
            if now >= next_refresh {
                self.refresh().await;
                if let Ok(n) = self.store.prune(crate::db::now() - KEEP_CYCLES * self.cycle_secs)
                    && n > 0
                {
                    self.history = self.store.load()?;
                }
                pace = self.pace();
                // Nothing testable yet (the source just started, say): try again soon.
                let any = self.nets.iter().any(|(_, st)| st.reference.is_some());
                next_refresh = now + if any { REFRESH_EVERY } else { Duration::from_secs(60) };
                tracing::info!(
                    "bench: {} masternodes to test, one start every {:.0} s",
                    self.testable_now().count(),
                    pace.as_secs_f64()
                );
            }
            if !sweep && now >= next_publish {
                self.publish(&mut full_publish).await;
                next_publish = Instant::now() + Duration::from_secs(self.args.publish_interval.max(60));
            }
            if !sweep && now >= next_snapshot {
                self.snapshot().await;
                next_snapshot = Instant::now() + SNAPSHOT_EVERY;
            }
            let bps = rate.sample(now, self.meter.load(Ordering::Relaxed));
            let limit_reached = self.args.limit.is_some_and(|l| started >= l);
            if tasks.len() < concurrency
                && now >= next_start
                && bps <= max_bps
                && !limit_reached
                && let Some(node) = self.pick(crate::db::now(), &in_flight, &swept)
            {
                if sweep {
                    swept.insert(node.protx.clone());
                }
                started += 1;
                let handle = tasks.spawn(self.test(node.clone()));
                in_flight.insert(handle.id(), node);
                next_start = if sweep { now } else { now + jittered(pace) };
                continue;
            }
            // A sweep is done once nothing is left to test, but not before the source has served
            // reference hashes: a sweep started with the app still warming up would end at once.
            let ready = self.nets.iter().any(|(_, st)| st.reference.is_some());
            if sweep
                && tasks.is_empty()
                && (limit_reached || (ready && self.pick(crate::db::now(), &in_flight, &swept).is_none()))
            {
                break;
            }
            if last_status.elapsed() >= Duration::from_secs(3600) {
                last_status = Instant::now();
                self.log_status();
            }
            tokio::select! {
                Some(joined) = tasks.join_next_with_id() => {
                    let (id, result) = match joined {
                        Ok((id, r)) => (id, Ok(r)),
                        Err(e) => (e.id(), Err(e.to_string())),
                    };
                    if let Some(node) = in_flight.remove(&id) {
                        // A test that panicked still counts as one, or it would be picked again at once.
                        let r = result.unwrap_or_else(|e| BenchResult {
                            ts: crate::db::now(),
                            vantage: self.vantage.clone(),
                            address: node.addr.to_string(),
                            error: Some(format!("internal error: {e}")),
                            ..Default::default()
                        });
                        if r.error.is_some() && !self.online(&node, &in_flight).await {
                            // The failure is ours, not the node's: drop it, so paced mode tests the node
                            // again soon (a sweep doesn't, and so still ends).
                            tracing::warn!(
                                "{} {}: not recorded, this vantage can't reach known-good masternodes either",
                                node.net.name(),
                                node.addr
                            );
                            next_start = next_start.max(Instant::now() + OFFLINE_PAUSE);
                        } else {
                            self.record(&node, r);
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
        if let Some(u) = &self.uploader {
            if u.token().is_some() {
                self.publish(&mut HashMap::new()).await;
                self.snapshot().await;
            } else {
                tracing::warn!("no bench token: results kept in {}, not published", self.args.state_dir.display());
            }
        }
        self.log_status();
        Ok(())
    }

    /// Whether this vantage can reach `failed`'s address network: one of a few masternodes there,
    /// on the same route, that passed their latest test (the most recent first, one per subnet, none
    /// under test now) still completes a handshake. Asked when a test fails, so an outage here (the
    /// uplink, a VPN, a proxy) isn't recorded as every masternode failing. An "online" answer is
    /// reused only while tests keep finishing cleanly after it, so an outage right after a check
    /// is still caught; "offline" holds for OFFLINE_PAUSE. With no masternode to compare with:
    /// online.
    async fn online(&mut self, failed: &Node, in_flight: &HashMap<tokio::task::Id, Node>) -> bool {
        let net = failed.addr.net();
        match self.online_check.get(&net) {
            Some(&(at, true)) if self.last_clean.get(&net).is_some_and(|&c| c > at) => return true,
            Some(&(at, false)) if at.elapsed() < OFFLINE_PAUSE => return false,
            _ => {}
        }
        let mut good: Vec<(i64, &Node)> = self
            .testable_now()
            .filter(|(n, _)| n.addr.net() == net && n.route == failed.route && n.protx != failed.protx)
            .filter(|(n, _)| !in_flight.values().any(|t| t.protx == n.protx))
            .filter_map(|(n, h)| h.first().filter(|r| r.ok && r.error.is_none()).map(|r| (r.ts, n)))
            .collect();
        good.sort_by_key(|g| std::cmp::Reverse(g.0));
        let mut subnets = HashSet::new();
        let mut checks = JoinSet::new();
        for (_, n) in good.into_iter().filter(|(_, n)| subnets.insert(&n.subnet)).take(ONLINE_CHECK_NODES) {
            let n = n.clone();
            checks.spawn(async move {
                let timeouts = transport::default_timeouts(n.addr.net());
                let t = probe::Target {
                    net: n.net,
                    addr: &n.addr,
                    route: &n.route,
                    timeouts: &timeouts,
                    expected_mns: None,
                };
                probe::handshakes(&t, ONLINE_CHECK_LIMIT).await
            });
        }
        let mut ok = checks.is_empty();
        while let Some(r) = checks.join_next().await {
            if r.unwrap_or(false) {
                ok = true;
                break;
            }
        }
        match (self.online_check.get(&net).is_none_or(|&(_, was)| was), ok) {
            (true, false) => tracing::warn!("bench: {net:?} seems unreachable from here; failed tests aren't recorded"),
            (false, true) => tracing::info!("bench: {net:?} is reachable again"),
            _ => {}
        }
        self.online_check.insert(net, (Instant::now(), ok));
        ok
    }

    fn record(&mut self, node: &Node, r: BenchResult) {
        log_result(node.net, &node.protx, &r);
        if r.error.is_none() {
            self.last_clean.insert(node.addr.net(), Instant::now());
        }
        if let Err(e) = self.store.insert(node.net, &node.protx, &r) {
            tracing::error!("bench state: {e:#}");
        }
        remember(&mut self.history, node.net, &node.protx, r);
    }

    /// One publishing round: each network's changed documents, or all of them once a day
    /// (`full_publish` remembers when, per network; a sweep passes an empty map and so a full round).
    /// Stops at the first failure: the Worker is likely down, and the rest can wait for the next round.
    async fn publish(&self, full_publish: &mut HashMap<Network, Instant>) {
        let Some(u) = &self.uploader else { return };
        let v = Vantage { id: &self.vantage, location: &self.args.vantage_location, cycle_days: self.args.cycle_days };
        for (net, _) in &self.nets {
            let force = full_publish.get(net).is_none_or(|t| t.elapsed() >= FULL_PUBLISH_EVERY);
            let docs = publish::documents(&self.history, *net, &v, crate::db::now());
            match u.publish(&self.store, &docs, force).await {
                Ok(n) => {
                    if n > 0 {
                        tracing::info!("{}: published {n} bench documents", net.name());
                    }
                    if force {
                        full_publish.insert(*net, Instant::now());
                    }
                }
                Err(e) => {
                    tracing::warn!("{}: publish: {e:#}", net.name());
                    return;
                }
            }
        }
    }

    async fn snapshot(&self) {
        if let Some((remote, name, path)) = &self.snapshots
            && let Err(e) = remote.save(name, path).await
        {
            tracing::warn!("snapshot {name}: {e:#}");
        }
    }

    fn log_status(&self) {
        let since = crate::db::now() - self.cycle_secs;
        for (net, st) in &self.nets {
            let tested = st
                .nodes
                .iter()
                .filter(|n| self.history.get(&(*net, n.protx.clone())).is_some_and(|h| tested_since(h, since)))
                .count();
            tracing::info!("{}: {tested} of {} masternodes tested this cycle", net.name(), st.nodes.len());
        }
    }
}

fn log_result(net: Network, protx: &str, r: &BenchResult) {
    let g = r.grade.as_ref();
    tracing::info!(
        "{} {} {}: {} {} ping {} ms, {} headers/s, mnlistdiff {} s{}",
        net.name(),
        protx.get(..12).unwrap_or(protx),
        r.address,
        g.map_or("-".into(), |g| g.letter.clone()),
        g.map_or("-".into(), |g| format!("{:.1}", g.score)),
        r.ping_ms.as_ref().and_then(|p| p.median).map_or("-".into(), |p| format!("{p:.0}")),
        r.headers.as_ref().map_or("-".into(), |h| format!("{:.0}", h.per_sec)),
        r.mnlistdiff.as_ref().map_or("-".into(), |m| format!("{:.2}", m.seconds)),
        r.error.as_ref().map_or(String::new(), |e| format!(" ({e})")),
    );
}

async fn once(args: &BenchArgs, source: &Source, addr: &str, params: &Params, vantage: &str) -> Result<()> {
    let net = Network::parse(&args.network).context("network must be mainnet or testnet")?;
    let addr = crate::resolve_addr(addr).await?;
    let route = args.transport.config().routes().require(addr.net())?;
    let reference = source.reference(net).await.context("reference hashes from SOURCE")?;
    let registered = match source.masternodes(net).await {
        Ok(l) => Some(l.len()),
        Err(e) => {
            tracing::warn!("masternode list: {e:#}; not checking the mnlistdiff size");
            None
        }
    };
    let timeouts = transport::default_timeouts(addr.net());
    let target = probe::Target { net, addr: &addr, route: &route, timeouts: &timeouts, expected_mns: registered };
    let r = probe::run(&target, &reference, params, vantage, &Meter::default()).await;
    println!("{}", serde_json::to_string_pretty(&r)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vantage_ids() {
        for ok in ["local", "nyc", "sgp-1", "a1", "abcdefghijklmnop"] {
            assert!(valid_vantage(ok), "{ok}");
        }
        for bad in ["", "x", "Local", "a_b", "abcdefghijklmnopq", "a/b", "../x"] {
            assert!(!valid_vantage(bad), "{bad}");
        }
    }

    #[test]
    fn only_enabled_reachable_masternodes() {
        let list: MnList = serde_json::from_value(serde_json::json!({"masternodes": [
            {"protx": "AA".repeat(32), "status": "enabled", "service": "1.2.3.4:9999"},
            {"protx": "bb".repeat(32), "status": "pose_banned", "service": "1.2.3.5:9999"},
            {"protx": "cc".repeat(32), "status": "enabled", "service": null},
            {"protx": "dd".repeat(32), "status": "enabled", "service": "10.0.0.1:9999"},
            {"protx": "ee".repeat(32), "status": "enabled", "service": "pg6mmjiyjmcrsslvykfwnntlaru7p5svn6y2ymmju6nubxndf4pscryd.onion:9999"},
            {"protx": "zz".repeat(32), "status": "enabled", "service": "1.2.3.6:9999"},
            {"protx": "ab", "status": "enabled", "service": "1.2.3.7:9999"},
        ]}))
        .unwrap();
        let routes = crate::transport::TransportConfig {
            ipv6: crate::transport::Toggle::Off,
            ipv6_proxy: None,
            tor_proxy: None,
            i2p_proxy: None,
            cjdns: crate::transport::Toggle::Off,
        }
        .routes();
        let nodes = testable(Network::Mainnet, &list.masternodes, &routes);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].protx, "aa".repeat(32));
        assert_eq!(nodes[0].subnet, "1.2/16");
        assert_eq!(nodes[0].addr.net(), AddrNet::Ipv4);
    }
}

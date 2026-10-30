mod addr;
mod api;
mod bench;
mod collector;
mod crawler;
mod db;
mod geo;
mod network;
mod p2p;
mod reference;
mod rpc;
mod socks5;
mod state;
mod transport;
mod ua;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::addr::{AddrNet, PeerAddr};
use crate::geo::Geo;
use crate::network::Network;
use crate::state::{CrawlConfig, NetCtx};
use crate::transport::{Toggle, TransportConfig};

#[derive(Parser)]
#[command(version, about = "Dash network crawler and masternode monitor")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the crawler, the dashd collectors and the web server.
    Serve(Box<ServeArgs>),
    /// Handshake with one node and print what it announces. Onion, I2P and CJDNS addresses go
    /// through the same proxies and routes as the crawler (TOR_PROXY, I2P_PROXY, ...).
    Probe {
        /// ip:port, [ipv6]:port, <name>.onion:port, <name>.b32.i2p:0 or host:port.
        addr: String,
        #[arg(long, default_value = "mainnet")]
        network: String,
        /// Also ask for addresses.
        #[arg(long)]
        getaddr: bool,
        #[command(flatten)]
        transport: TransportArgs,
    },
    /// Speed-test masternodes (headers, ping, masternode list) and publish the results. Needs no
    /// dashd: targets and reference hashes come from SOURCE. See bench/mod.rs.
    Bench(Box<bench::BenchArgs>),
}

/// How to reach each address network. See transport.rs.
#[derive(Args)]
pub(crate) struct TransportArgs {
    /// Crawl IPv6: auto (if this host has a global IPv6 route, or IPV6_PROXY is set), true, false.
    #[arg(long, env = "CRAWL_IPV6", default_value = "auto")]
    crawl_ipv6: Toggle,
    /// SOCKS5 proxy (host:port) for IPv6 peers, for hosts without IPv6.
    #[arg(long, env = "IPV6_PROXY")]
    ipv6_proxy: Option<String>,
    /// Tor SOCKS5 proxy (host:port) for .onion peers.
    #[arg(long, env = "TOR_PROXY")]
    tor_proxy: Option<String>,
    /// I2P SOCKS5 proxy (host:port, i2pd's socksproxy) for .b32.i2p peers.
    #[arg(long, env = "I2P_PROXY")]
    i2p_proxy: Option<String>,
    /// Crawl CJDNS (fc00::/8) directly: auto (if a local cjdns interface routes it), true, false.
    #[arg(long, env = "CJDNS", default_value = "auto")]
    cjdns: Toggle,
}

impl TransportArgs {
    pub(crate) fn config(&self) -> TransportConfig {
        TransportConfig {
            ipv6: self.crawl_ipv6,
            ipv6_proxy: self.ipv6_proxy.clone(),
            tor_proxy: self.tor_proxy.clone(),
            i2p_proxy: self.i2p_proxy.clone(),
            cjdns: self.cjdns,
        }
    }
}

#[derive(Args)]
struct ServeArgs {
    #[arg(long, env = "DATA_DIR", default_value = "./data")]
    data_dir: PathBuf,
    #[arg(long, env = "WEB_DIR", default_value = "./web")]
    web_dir: PathBuf,
    #[arg(long, env = "LISTEN", default_value = "0.0.0.0:8080")]
    listen: String,
    /// dashd JSON-RPC URL for mainnet; leave unset to skip mainnet.
    #[arg(long, env = "MAINNET_RPC")]
    mainnet_rpc: Option<String>,
    #[arg(long, env = "TESTNET_RPC")]
    testnet_rpc: Option<String>,
    #[arg(long, env = "RPC_USER", default_value = "dashnodes")]
    rpc_user: String,
    #[arg(long, env = "RPC_PASSWORD_FILE")]
    rpc_password_file: Option<PathBuf>,
    #[arg(long, env = "RPC_PASSWORD", hide_env_values = true)]
    rpc_password: Option<String>,
    /// Seconds between crawl starts.
    #[arg(long, env = "CRAWL_INTERVAL", default_value_t = 900)]
    crawl_interval: u64,
    /// Parallel handshakes for each directly reached network (IPv4, IPv6; CJDNS at most 32).
    #[arg(long, env = "CRAWL_CONCURRENCY", default_value_t = 128)]
    crawl_concurrency: usize,
    /// Parallel handshakes through Tor.
    #[arg(long, env = "TOR_CONCURRENCY", default_value_t = 16)]
    tor_concurrency: usize,
    /// Parallel handshakes through I2P.
    #[arg(long, env = "I2P_CONCURRENCY", default_value_t = 8)]
    i2p_concurrency: usize,
    #[command(flatten)]
    transport: TransportArgs,
    #[arg(long, env = "GETADDR_PER_CRAWL", default_value_t = 200)]
    getaddr_per_crawl: usize,
    /// Lowest height to backfill masternode payments to (default: DIP3 enforcement).
    #[arg(long, env = "PAYMENTS_FLOOR_MAINNET")]
    payments_floor_mainnet: Option<u32>,
    #[arg(long, env = "PAYMENTS_FLOOR_TESTNET")]
    payments_floor_testnet: Option<u32>,
    /// Pause between backfill batches, to leave dashd room for everything else.
    #[arg(long, env = "BACKFILL_PAUSE_MS", default_value_t = 200)]
    backfill_pause_ms: u64,
}

/// Runs a background task for a network, restarting it if it panics. A task that returns is done.
fn supervise<F, Fut>(name: &'static str, ctx: &Arc<NetCtx>, task: F)
where
    F: Fn(Arc<NetCtx>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let ctx = ctx.clone();
    tokio::spawn(async move {
        while let Err(e) = tokio::spawn(task(ctx.clone())).await {
            tracing::error!(net = ctx.net.name(), "{name} panicked ({e}); restarting in 30s");
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,dash_nodes=info".into()),
        )
        .init();
    match Cli::parse().cmd {
        Cmd::Serve(args) => serve(*args).await,
        Cmd::Probe { addr, network, getaddr, transport } => probe(&addr, &network, getaddr, &transport).await,
        Cmd::Bench(args) => bench::main(*args).await,
    }
}

async fn probe(addr: &str, network: &str, getaddr: bool, transport: &TransportArgs) -> Result<()> {
    let net = Network::parse(network).context("network must be mainnet or testnet")?;
    let addr = resolve_addr(addr).await?;
    let route = transport.config().routes().require(addr.net())?;
    let started = Instant::now();
    let p = p2p::probe(net, &addr, &route, 0, getaddr, &transport::default_timeouts(addr.net())).await?;
    println!("{addr} ({}, via {route:?}) in {:.1?}", addr.net(), started.elapsed());
    println!("{:#?}", p.version);
    if getaddr {
        let mut by_net = BTreeMap::new();
        for a in &p.addrs {
            *by_net.entry(a.net()).or_insert(0) += 1;
        }
        println!("{} addresses: {by_net:?}", p.addrs.len());
        for a in p.addrs.iter().filter(|a| a.net() != AddrNet::Ipv4) {
            println!("  {a}");
        }
    }
    Ok(())
}

/// A peer address from the command line: an address in any of our forms, or a host name to resolve.
pub(crate) async fn resolve_addr(addr: &str) -> Result<PeerAddr> {
    match addr.parse() {
        Ok(a) => Ok(a),
        // A mistyped onion or I2P name must not go to the system resolver.
        Err(e) if addr.rsplit_once(':').is_some_and(|(h, _)| h.ends_with(".onion") || h.ends_with(".i2p")) => Err(e),
        Err(_) => Ok(PeerAddr::from_socket(tokio::net::lookup_host(addr).await?.next().context("no address")?)),
    }
}

async fn serve(args: ServeArgs) -> Result<()> {
    let password = match (&args.rpc_password_file, &args.rpc_password) {
        (Some(f), _) => std::fs::read_to_string(f).with_context(|| format!("read {}", f.display()))?.trim().to_string(),
        (None, Some(p)) => p.clone(),
        (None, None) => bail!("set RPC_PASSWORD_FILE or RPC_PASSWORD"),
    };
    std::fs::create_dir_all(&args.data_dir)?;
    let geo = Geo::default();
    tokio::spawn(geo.clone().run(args.data_dir.join("geo")));

    let crawl_cfg = CrawlConfig {
        interval_secs: args.crawl_interval,
        transport: args.transport.config(),
        limits: transport::default_limits(
            args.crawl_concurrency.max(1),
            args.tor_concurrency.max(1),
            args.i2p_concurrency.max(1),
        ),
        getaddr_per_crawl: args.getaddr_per_crawl,
    };
    let routes = crawl_cfg.transport.routes();
    for net in AddrNet::ALL {
        let r = routes.get(net);
        match &r.route {
            Some(route) => tracing::info!("{net}: crawled via {route:?}"),
            None => tracing::info!("{net}: not crawled ({})", r.reason.as_deref().unwrap_or("")),
        }
    }
    let mut nets = HashMap::new();
    for (net, url, floor) in [
        (Network::Mainnet, &args.mainnet_rpc, args.payments_floor_mainnet),
        (Network::Testnet, &args.testnet_rpc, args.payments_floor_testnet),
    ] {
        let Some(url) = url else { continue };
        let db = db::Db::open(&args.data_dir.join(format!("{}.sqlite", net.name())))?;
        let ctx = Arc::new(NetCtx {
            net,
            db,
            rpc: rpc::Rpc::new(url.clone(), args.rpc_user.clone(), password.clone()),
            geo: geo.clone(),
            status: RwLock::default(),
            crawl_status: RwLock::default(),
            mn_loaded: AtomicBool::new(false),
            crawl_cfg: crawl_cfg.clone(),
            backfill_pause_ms: args.backfill_pause_ms,
            pay_lock: tokio::sync::Mutex::new(()),
            reference: RwLock::default(),
        });
        let floor = floor.unwrap_or(net.dip3_enforcement_height());
        supervise("collector", &ctx, collector::run);
        supervise("backfill", &ctx, move |c| collector::backfill(c, floor));
        supervise("crawler", &ctx, crawler::run);
        supervise("reference", &ctx, reference::run);
        tracing::info!("{}: dashd at {url}", net.name());
        nets.insert(net, ctx);
    }
    if nets.is_empty() {
        bail!("no network configured: set MAINNET_RPC and/or TESTNET_RPC");
    }
    let app = api::router(Arc::new(nets), &args.web_dir);
    let listener = tokio::net::TcpListener::bind(&args.listen).await?;
    tracing::info!("listening on {}", args.listen);
    axum::serve(listener, app).await?;
    Ok(())
}

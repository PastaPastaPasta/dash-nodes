mod api;
mod collector;
mod crawler;
mod db;
mod geo;
mod network;
mod p2p;
mod rpc;
mod state;
mod ua;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::geo::Geo;
use crate::network::Network;
use crate::p2p::Timeouts;
use crate::state::{CrawlConfig, NetCtx};

#[derive(Parser)]
#[command(version, about = "Dash network crawler and masternode monitor")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the crawler, the dashd collectors and the web server.
    Serve(ServeArgs),
    /// Handshake with one node and print what it announces.
    Probe {
        addr: String,
        #[arg(long, default_value = "mainnet")]
        network: String,
        /// Also ask for addresses.
        #[arg(long)]
        getaddr: bool,
    },
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
    #[arg(long, env = "CRAWL_CONCURRENCY", default_value_t = 128)]
    crawl_concurrency: usize,
    #[arg(long, env = "CRAWL_IPV6", default_value_t = false)]
    crawl_ipv6: bool,
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

/// Runs a background task, restarting it if it panics. A task that returns is done.
fn supervise<F, Fut>(name: &'static str, net: Network, task: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        while let Err(e) = tokio::spawn(task()).await {
            tracing::error!(net = net.name(), "{name} panicked ({e}); restarting in 30s");
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
}

fn timeouts() -> Timeouts {
    Timeouts { connect: Duration::from_secs(5), handshake: Duration::from_secs(10), getaddr: Duration::from_secs(10) }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,dash_nodes=info".into()),
        )
        .init();
    match Cli::parse().cmd {
        Cmd::Serve(args) => serve(args).await,
        Cmd::Probe { addr, network, getaddr } => {
            let net = Network::parse(&network).context("network must be mainnet or testnet")?;
            let addr = tokio::net::lookup_host(&addr).await?.next().context("no address")?;
            let p = p2p::probe(net, addr, 0, getaddr, &timeouts()).await?;
            println!("{:#?}", p.version);
            if getaddr {
                println!("{} addresses", p.addrs.len());
            }
            Ok(())
        }
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
        concurrency: args.crawl_concurrency,
        timeouts: timeouts(),
        ipv6: args.crawl_ipv6,
        getaddr_per_crawl: args.getaddr_per_crawl,
    };
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
        });
        let floor = floor.unwrap_or(net.dip3_enforcement_height());
        supervise("collector", net, { let c = ctx.clone(); move || collector::run(c.clone()) });
        supervise("backfill", net, { let c = ctx.clone(); move || collector::backfill(c.clone(), floor) });
        supervise("crawler", net, { let c = ctx.clone(); move || crawler::run(c.clone()) });
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

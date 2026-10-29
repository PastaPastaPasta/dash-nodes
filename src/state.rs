use std::net::SocketAddr;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;

use serde::Serialize;

use crate::collector::ChainStatus;
use crate::db::Db;
use crate::geo::Geo;
use crate::network::Network;
use crate::p2p::{self, Timeouts};
use crate::rpc::Rpc;

/// Everything the background tasks and API handlers share for one network.
pub struct NetCtx {
    pub net: Network,
    pub db: Db,
    pub rpc: Rpc,
    pub geo: Geo,
    pub status: RwLock<ChainStatus>,
    pub crawl_status: RwLock<CrawlStatus>,
    /// Set once the masternode list has been loaded; crawls wait for it so MN stats are right.
    pub mn_loaded: AtomicBool,
    pub crawl_cfg: CrawlConfig,
    pub backfill_pause_ms: u64,
    /// Serializes the forward indexer and the backfill (see collector.rs).
    pub pay_lock: tokio::sync::Mutex<()>,
}

#[derive(Clone, Debug)]
pub struct CrawlConfig {
    pub interval_secs: u64,
    pub concurrency: usize,
    pub timeouts: Timeouts,
    pub ipv6: bool,
    /// Max peers asked for addresses per crawl; each one is asked at most every 6 hours.
    pub getaddr_per_crawl: usize,
}

impl CrawlConfig {
    /// Whether an address is worth probing: publicly routable, and IPv4 unless IPv6 crawling is on.
    pub fn usable(&self, addr: &SocketAddr) -> bool {
        p2p::is_routable(addr) && (self.ipv6 || addr.is_ipv4())
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CrawlStatus {
    pub running: bool,
    pub last_id: Option<i64>,
    pub last_finished: Option<i64>,
    pub last_error: Option<String>,
}

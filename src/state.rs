use std::sync::RwLock;
use std::sync::atomic::AtomicBool;

use serde::Serialize;

use crate::addr::AddrNet;
use crate::collector::ChainStatus;
use crate::db::Db;
use crate::geo::Geo;
use crate::network::Network;
use crate::reference::Reference;
use crate::rpc::Rpc;
use crate::transport::{NetLimits, TransportConfig};

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
    /// Block hashes the bench checks headers against (see reference.rs); None until dashd is synced.
    pub reference: RwLock<Option<Reference>>,
}

#[derive(Clone, Debug)]
pub struct CrawlConfig {
    pub interval_secs: u64,
    pub transport: TransportConfig,
    /// Timeouts and parallel probes, per address network (`AddrNet::index`).
    pub limits: [NetLimits; AddrNet::COUNT],
    /// Max peers asked for addresses per crawl; each one is asked at most every 6 hours.
    pub getaddr_per_crawl: usize,
}

impl CrawlConfig {
    pub fn limits(&self, net: AddrNet) -> &NetLimits {
        &self.limits[net.index()]
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CrawlStatus {
    pub running: bool,
    pub last_id: Option<i64>,
    pub last_finished: Option<i64>,
    pub last_error: Option<String>,
}

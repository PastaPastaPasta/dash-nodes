//! The bench's own state in `STATE_DIR/bench.sqlite`: the latest results per masternode (which also
//! drive scheduling) and what has been published, so a restart neither re-tests nor re-uploads.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use super::probe::BenchResult;
use crate::network::Network;

/// Results kept (and published) per masternode, newest first.
const KEEP_PER_MN: usize = 6;

/// Results per (network, ProTx hash), newest first.
pub type History = HashMap<(Network, String), Vec<BenchResult>>;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS results (
    net TEXT NOT NULL,
    protx TEXT NOT NULL,
    ts INTEGER NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (net, protx, ts)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS published (
    key TEXT PRIMARY KEY,
    hash TEXT NOT NULL
) WITHOUT ROWID;
";

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(dir: &Path) -> Result<Store> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let path = dir.join("bench.sqlite");
        let conn = Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn })
    }

    pub fn load(&self) -> Result<History> {
        let mut out = History::new();
        let mut st = self.conn.prepare("SELECT net, protx, json FROM results ORDER BY ts DESC")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let net: String = r.get(0)?;
            let Some(net) = Network::parse(&net) else { continue };
            let json: String = r.get(2)?;
            // A row an older build wrote in another shape: skip it rather than refuse to start.
            let Ok(result) = serde_json::from_str::<BenchResult>(&json) else { continue };
            out.entry((net, r.get(1)?)).or_default().push(result);
        }
        Ok(out)
    }

    /// Stores a result and drops that masternode's results beyond the newest KEEP_PER_MN.
    pub fn insert(&self, net: Network, protx: &str, r: &BenchResult) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO results (net, protx, ts, json) VALUES (?1, ?2, ?3, ?4)",
            params![net.name(), protx, r.ts, serde_json::to_string(r)?],
        )?;
        self.conn.execute(
            "DELETE FROM results WHERE net = ?1 AND protx = ?2 AND ts NOT IN
                (SELECT ts FROM results WHERE net = ?1 AND protx = ?2 ORDER BY ts DESC LIMIT ?3)",
            params![net.name(), protx, KEEP_PER_MN as i64],
        )?;
        Ok(())
    }

    /// Forgets results older than `before` (masternodes that left the list stop being published).
    pub fn prune(&self, before: i64) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM results WHERE ts < ?1", [before])?)
    }

    pub fn published(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT hash FROM published WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_published(&self, key: &str, hash: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO published (key, hash) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET hash = excluded.hash",
            params![key, hash],
        )?;
        Ok(())
    }
}

/// Adds a result to the in-memory history the same way `Store::insert` does on disk.
pub fn remember(history: &mut History, net: Network, protx: &str, r: BenchResult) {
    let list = history.entry((net, protx.to_owned())).or_default();
    list.insert(0, r);
    list.sort_by_key(|r| std::cmp::Reverse(r.ts));
    list.truncate(KEEP_PER_MN);
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub fn result(ts: i64, ok: bool) -> BenchResult {
        BenchResult { ts, ok, vantage: "test".into(), address: "1.2.3.4:9999".into(), ..Default::default() }
    }

    #[test]
    fn keeps_newest_six_and_survives_reopen() {
        let dir = std::env::temp_dir().join(format!("bench-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let s = Store::open(&dir).unwrap();
            for ts in 1..=8 {
                s.insert(Network::Mainnet, "aa", &result(ts, ts % 2 == 0)).unwrap();
            }
            s.insert(Network::Testnet, "aa", &result(5, true)).unwrap();
            s.set_published("k", "h1").unwrap();
            s.set_published("k", "h2").unwrap();
        }
        let s = Store::open(&dir).unwrap();
        let h = s.load().unwrap();
        let main: Vec<i64> = h[&(Network::Mainnet, "aa".into())].iter().map(|r| r.ts).collect();
        assert_eq!(main, [8, 7, 6, 5, 4, 3]);
        assert_eq!(h[&(Network::Testnet, "aa".into())].len(), 1);
        assert_eq!(s.published("k").unwrap().as_deref(), Some("h2"));
        assert_eq!(s.published("nope").unwrap(), None);
        // Mainnet's 3, 4, 5 and testnet's 5.
        assert_eq!(s.prune(6).unwrap(), 4);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn remember_keeps_order_and_bound() {
        let mut h = History::new();
        for ts in [3, 1, 2, 9, 8, 7, 6, 5] {
            remember(&mut h, Network::Mainnet, "bb", result(ts, true));
        }
        let ts: Vec<i64> = h[&(Network::Mainnet, "bb".into())].iter().map(|r| r.ts).collect();
        assert_eq!(ts, [9, 8, 7, 6, 5, 3]);
    }
}

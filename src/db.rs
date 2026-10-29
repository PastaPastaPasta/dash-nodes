//! SQLite storage, one database file per network.
//!
//! Background tasks write through a single connection; API handlers read through a small pool of
//! read-only connections (WAL lets them run while a crawl transaction is open).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Every P2P address we know of, whether or not it ever answered.
CREATE TABLE IF NOT EXISTS nodes (
    addr          TEXT PRIMARY KEY,          -- ip:port, [v6]:port
    source        TEXT NOT NULL,             -- dashd | getaddr | seed | mnlist
    first_seen    INTEGER NOT NULL,
    last_gossip   INTEGER,                   -- last time a peer or dashd told us about it
    last_tried    INTEGER,
    last_success  INTEGER,
    fail_streak   INTEGER NOT NULL DEFAULT 0,
    next_try      INTEGER NOT NULL DEFAULT 0,
    last_getaddr  INTEGER,
    last_crawl    INTEGER,                   -- crawls.id of the last probe
    last_ok       INTEGER NOT NULL DEFAULT 0,-- 1 if that probe succeeded
    last_error    TEXT,
    user_agent    TEXT,
    protocol      INTEGER,
    services      INTEGER,
    start_height  INTEGER,
    latency_ms    INTEGER,
    country       TEXT,
    asn           INTEGER,
    as_org        TEXT
);
CREATE INDEX IF NOT EXISTS nodes_next_try ON nodes(next_try);
CREATE INDEX IF NOT EXISTS nodes_last_crawl ON nodes(last_crawl);

CREATE TABLE IF NOT EXISTS crawls (
    id           INTEGER PRIMARY KEY,
    started      INTEGER NOT NULL,
    finished     INTEGER,
    tip_height   INTEGER,
    tried        INTEGER,
    reachable    INTEGER,
    discovered   INTEGER,
    mn_total     INTEGER,
    mn_reachable INTEGER
);

-- Distribution counts per crawl, kept for a few weeks, then only as daily averages.
CREATE TABLE IF NOT EXISTS crawl_agg (
    crawl_id INTEGER NOT NULL,
    scope    TEXT NOT NULL,                  -- all | nonmn | mn | evo
    dim      TEXT NOT NULL,                  -- ua | version | protocol | country | asn | reachable | status | type
    key      TEXT NOT NULL,
    count    INTEGER NOT NULL,
    PRIMARY KEY (crawl_id, scope, dim, key)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS daily_agg (
    day   TEXT NOT NULL,                     -- YYYY-MM-DD (UTC)
    scope TEXT NOT NULL,
    dim   TEXT NOT NULL,
    key   TEXT NOT NULL,
    avg   REAL NOT NULL,
    PRIMARY KEY (day, scope, dim, key)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS masternodes (
    protx                   TEXT PRIMARY KEY,
    type                    TEXT NOT NULL,   -- Regular | Evo
    collateral              TEXT,            -- txid-index
    collateral_address      TEXT,
    operator_reward         REAL,
    service                 TEXT,            -- core P2P ip:port
    platform_p2p            TEXT,
    platform_https          TEXT,
    platform_node_id        TEXT,
    payout_address          TEXT,
    operator_payout_address TEXT,
    owner_address           TEXT,
    voting_address          TEXT,
    pubkey_operator         TEXT,
    registered_height       INTEGER,
    last_paid_height        INTEGER,
    consecutive_payments    INTEGER,
    pose_penalty            INTEGER,
    pose_revived_height     INTEGER,
    pose_ban_height         INTEGER,
    revocation_reason       INTEGER,
    first_seen              INTEGER NOT NULL,
    updated                 INTEGER NOT NULL,
    removed                 INTEGER          -- set when the ProTx left the registered list
);
CREATE INDEX IF NOT EXISTS masternodes_service ON masternodes(service);

-- Port-check results per masternode per UTC day (uptime).
CREATE TABLE IF NOT EXISTS mn_daily (
    protx    TEXT NOT NULL,
    day      TEXT NOT NULL,
    attempts INTEGER NOT NULL,
    ok       INTEGER NOT NULL,
    PRIMARY KEY (protx, day)
) WITHOUT ROWID;

-- One row each time a masternode's announced user agent or protocol changes.
CREATE TABLE IF NOT EXISTS mn_versions (
    protx      TEXT NOT NULL,
    ts         INTEGER NOT NULL,
    user_agent TEXT NOT NULL,
    protocol   INTEGER NOT NULL,
    PRIMARY KEY (protx, ts)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS blocks (
    height    INTEGER PRIMARY KEY,
    hash      TEXT NOT NULL,
    time      INTEGER NOT NULL,
    mn_amount INTEGER NOT NULL               -- duffs paid to masternodes in this block
);

CREATE TABLE IF NOT EXISTS payments (
    height INTEGER NOT NULL,
    protx  TEXT NOT NULL,
    amount INTEGER NOT NULL,                 -- duffs
    PRIMARY KEY (height, protx)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS payments_protx ON payments(protx, height);
"#;

const READERS: usize = 8;

#[derive(Clone)]
pub struct Db {
    path: PathBuf,
    writer: Arc<Mutex<Connection>>,
    readers: Arc<Mutex<Vec<Connection>>>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.execute_batch(SCHEMA).context("schema")?;
        Ok(Db { path: path.to_owned(), writer: Arc::new(Mutex::new(conn)), readers: Arc::new(Mutex::new(Vec::new())) })
    }

    pub async fn write<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut Connection) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let writer = self.writer.clone();
        tokio::task::spawn_blocking(move || f(&mut writer.lock().unwrap())).await?
    }

    pub async fn read<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Connection) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let db = self.clone();
        tokio::task::spawn_blocking(move || {
            let conn = match db.readers.lock().unwrap().pop() {
                Some(c) => c,
                None => {
                    let c = Connection::open_with_flags(&db.path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                    c.busy_timeout(std::time::Duration::from_secs(10))?;
                    c
                }
            };
            let result = f(&conn);
            let mut pool = db.readers.lock().unwrap();
            if pool.len() < READERS {
                pool.push(conn);
            }
            result
        })
        .await?
    }
}

pub fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()?)
}

pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// UTC calendar day of a unix timestamp, as YYYY-MM-DD.
pub fn day_of(ts: i64) -> String {
    // Civil-from-days (Howard Hinnant), avoids pulling in a date crate for one function.
    let z = ts.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days() {
        assert_eq!(day_of(0), "1970-01-01");
        assert_eq!(day_of(951_782_400), "2000-02-29");
        assert_eq!(day_of(1_793_318_399), "2026-10-29");
        assert_eq!(day_of(1_793_318_400), "2026-10-30");
    }
}

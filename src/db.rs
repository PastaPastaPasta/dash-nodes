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
    addr          TEXT PRIMARY KEY,          -- ip:port, [v6]:port, x.onion:port, x.b32.i2p:0, [fc..]:port
    network       TEXT,                      -- ipv4 | ipv6 | onion | i2p | cjdns
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
    relay         INTEGER,                   -- version message relay flag (0 with blocksonly=1)
    start_height  INTEGER,
    latency_ms    INTEGER,
    country       TEXT,
    asn           INTEGER,
    as_org        TEXT,
    continent     TEXT,
    city          TEXT,
    lat           REAL,
    lon           REAL
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
    mn_reachable INTEGER,
    networks     TEXT                        -- JSON: per address network counts and crawl status
);

-- Distribution counts per crawl, kept for a few weeks, then only as daily averages.
CREATE TABLE IF NOT EXISTS crawl_agg (
    crawl_id INTEGER NOT NULL,
    scope    TEXT NOT NULL,                  -- all | nonmn | mn | evo
    dim      TEXT NOT NULL,                  -- ua | version | protocol | country | asn | network | reachable | status | type
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
    service                 TEXT,            -- primary core P2P address (the first of core_p2p)
    core_p2p                TEXT,            -- JSON array of every core P2P address (v24 ExtNetInfo)
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
    platform_banned         INTEGER,         -- dashd's metaInfo.is_platform_banned (evonodes)
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

-- One row per change to a masternode's registration, as the list sync sees it: `field` is the
-- column that changed (service, payout_address, …), `pose` (enabled/banned), `registered` or
-- `removed`; old and new are its values as text.
CREATE TABLE IF NOT EXISTS mn_changes (
    id     INTEGER PRIMARY KEY,
    ts     INTEGER NOT NULL,
    height INTEGER,
    protx  TEXT NOT NULL,
    field  TEXT NOT NULL,
    old    TEXT,
    new    TEXT
);
CREATE INDEX IF NOT EXISTS mn_changes_protx ON mn_changes(protx, ts);
CREATE INDEX IF NOT EXISTS mn_changes_ts ON mn_changes(ts);

-- Evonode Platform health (platform.rs): the latest check of each evonode, and daily counts of
-- checks where both endpoints answered as that evonode.
CREATE TABLE IF NOT EXISTS platform_status (
    protx                  TEXT PRIMARY KEY,
    ts                     INTEGER NOT NULL,
    p2p_ms                 INTEGER,
    p2p_error              TEXT,
    https_ms               INTEGER,
    https_error            TEXT,
    cert_issuer            TEXT,
    cert_expires           INTEGER,
    dapi                   TEXT,
    drive                  TEXT,
    tenderdash             TEXT,
    drive_protocol_latest  INTEGER,
    drive_protocol_current INTEGER,
    height                 INTEGER,
    catching_up            INTEGER,
    peers                  INTEGER,
    node_protx             TEXT,
    drive_protocol_next    INTEGER                -- the version Drive runs from the next epoch (a locked-in upgrade)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS platform_daily (
    protx    TEXT NOT NULL,
    day      TEXT NOT NULL,
    attempts INTEGER NOT NULL,
    ok       INTEGER NOT NULL,
    PRIMARY KEY (protx, day)
) WITHOUT ROWID;

-- Platform rewards (rewards.rs). Credits: 1 DASH = 1e11. Every finalized epoch, as Drive recorded it
-- when it paid the epoch out (getFinalizedEpochInfos; there are none before epoch 34 on mainnet).
CREATE TABLE IF NOT EXISTS platform_epochs (
    epoch              INTEGER PRIMARY KEY,
    first_block_height INTEGER NOT NULL,
    first_block_time   INTEGER NOT NULL,     -- unix seconds
    first_core_height  INTEGER NOT NULL,
    next_core_height   INTEGER NOT NULL,
    total_blocks       INTEGER NOT NULL,
    processing_fees    INTEGER NOT NULL,
    storage_fees       INTEGER NOT NULL,     -- distributed storage fees (this epoch's share)
    core_rewards       INTEGER NOT NULL,
    protocol_version   INTEGER NOT NULL,
    proposers          INTEGER NOT NULL
);
-- What each proposer was paid for an epoch, with Drive's integer rules (rewards::payouts).
CREATE TABLE IF NOT EXISTS platform_payouts (
    epoch   INTEGER NOT NULL,
    protx   TEXT NOT NULL,
    blocks  INTEGER NOT NULL,
    credits INTEGER NOT NULL,
    PRIMARY KEY (epoch, protx)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS platform_payouts_protx ON platform_payouts(protx, epoch);
-- Blocks each evonode proposed so far in the current epoch (replaced every poll).
CREATE TABLE IF NOT EXISTS platform_epoch_blocks (
    protx  TEXT PRIMARY KEY,
    epoch  INTEGER NOT NULL,
    blocks INTEGER NOT NULL
) WITHOUT ROWID;
-- Each evonode's identity balance (claimable credits), and a row whenever it changes: a step up is a
-- payout, a drop a withdrawal or transfer.
CREATE TABLE IF NOT EXISTS platform_balances (
    protx   TEXT PRIMARY KEY,
    credits INTEGER,                         -- NULL: no identity
    ts      INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS platform_balance_history (
    protx   TEXT NOT NULL,
    ts      INTEGER NOT NULL,
    credits INTEGER NOT NULL,
    PRIMARY KEY (protx, ts)
) WITHOUT ROWID;
-- The protocol version each evonode last voted for (getProtocolVersionUpgradeVoteStatus).
CREATE TABLE IF NOT EXISTS platform_votes (
    protx   TEXT PRIMARY KEY,
    version INTEGER NOT NULL,
    ts      INTEGER NOT NULL
) WITHOUT ROWID;
-- Proposer turns per evonode and epoch, from sampling getCurrentQuorumsInfo: `memberships` sums, over
-- samples, the rotating validator sets it was an active member of (its expected share of blocks);
-- `turns` are turns we saw it take or miss, `missed` the missed ones.
CREATE TABLE IF NOT EXISTS platform_turns (
    epoch       INTEGER NOT NULL,
    protx       TEXT NOT NULL,
    memberships INTEGER NOT NULL DEFAULT 0,
    turns       INTEGER NOT NULL DEFAULT 0,
    missed      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (epoch, protx)
) WITHOUT ROWID;
-- Per epoch: quorum samples taken and the sum of every member's memberships over them.
CREATE TABLE IF NOT EXISTS platform_turn_samples (
    epoch       INTEGER PRIMARY KEY,
    samples     INTEGER NOT NULL,
    memberships INTEGER NOT NULL
);
-- Platform Explorer's lifetime reward total per evonode (third party, unverified), for epochs before 34.
CREATE TABLE IF NOT EXISTS platform_explorer (
    protx        TEXT PRIMARY KEY,
    total_reward INTEGER,                    -- credits; NULL: not known there
    epoch        INTEGER,                    -- its current epoch when fetched
    fetched      INTEGER NOT NULL
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
    prev      TEXT NOT NULL,
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
-- Time to height (realized returns turn "the last 90 days" into a height range).
CREATE INDEX IF NOT EXISTS blocks_time ON blocks(time);
"#;

const READERS: usize = 8;

#[derive(Clone)]
pub struct Db {
    path: PathBuf,
    writer: Arc<Mutex<Connection>>,
    readers: Arc<Mutex<Vec<Connection>>>,
    /// Caps concurrent reads, so a burst of API requests queues instead of piling up threads.
    read_slots: Arc<tokio::sync::Semaphore>,
}

/// Columns added after the first release, for databases created before them: (table, column, type).
const ADDED_COLUMNS: [(&str, &str, &str); 10] = [
    ("nodes", "continent", "TEXT"),
    ("nodes", "city", "TEXT"),
    ("nodes", "lat", "REAL"),
    ("nodes", "lon", "REAL"),
    ("nodes", "network", "TEXT"),
    ("masternodes", "core_p2p", "TEXT"),
    ("crawls", "networks", "TEXT"),
    ("nodes", "relay", "INTEGER"),
    ("masternodes", "platform_banned", "INTEGER"),
    ("platform_status", "drive_protocol_next", "INTEGER"),
];

/// Every core P2P address of every registered masternode (the primary `service` for rows the
/// collector hasn't refreshed since `core_p2p` was added).
const MN_ADDRS_VIEW: &str = "
DROP VIEW IF EXISTS mn_addrs;
CREATE VIEW mn_addrs AS
    SELECT m.protx, j.value AS addr FROM masternodes m,
        json_each(CASE WHEN m.core_p2p IS NOT NULL THEN m.core_p2p
                       WHEN m.service IS NOT NULL THEN json_array(m.service) ELSE '[]' END) j
    WHERE m.removed IS NULL;
";

fn migrate(c: &mut Connection) -> Result<()> {
    for (table, column, ty) in ADDED_COLUMNS {
        let exists: bool = c.query_row(
            &format!("SELECT count(*) > 0 FROM pragma_table_info('{table}') WHERE name = ?1"),
            [column],
            |r| r.get(0),
        )?;
        if !exists {
            c.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {ty}"))?;
        }
    }
    c.execute_batch(MN_ADDRS_VIEW)?;
    c.execute_batch("CREATE INDEX IF NOT EXISTS nodes_network ON nodes(network)")?;
    // Classify rows from before the network column (all of them IPv4 or IPv6 then).
    let unclassified: Vec<String> = query_all(c, "SELECT addr FROM nodes WHERE network IS NULL", [], |r| r.get(0))?;
    if !unclassified.is_empty() {
        let tx = c.transaction()?;
        {
            let mut st = tx.prepare("UPDATE nodes SET network = ?2 WHERE addr = ?1")?;
            for addr in &unclassified {
                st.execute(params![addr, crate::addr::net_of(addr).map(|n| n.name())])?;
            }
        }
        tx.commit()?;
    }
    Ok(())
}

/// `PRAGMA user_version` once payments no longer count the platform share (see `drop_platform_share`).
const NO_PLATFORM_SHARE: i64 = 1;
/// Dash's block subsidy steps down every this many blocks.
const SUBSIDY_ERA: i64 = 210_240;

/// What the coinbase at `height` (on or after MN_RR) sends to Platform's credit pool: Core's
/// `PlatformShare(GetMasternodePayment(height, subsidy))`. After V20 the subsidy starts at 5 DASH and
/// loses 1/14 every SUBSIDY_ERA blocks (counted from the parent's height); the treasury takes 20% of it,
/// masternodes 75% of the rest, and Platform 37.5% of theirs. Fees don't count.
pub fn platform_share(height: i64) -> i64 {
    let mut subsidy: i64 = 500_000_000;
    for _ in 0..(height - 1) / SUBSIDY_ERA {
        subsidy -= subsidy / 14;
    }
    (subsidy - subsidy / 5) * 3 / 4 * 375 / 1000
}

/// Takes the platform share out of payments recorded before the collector stopped counting it (as
/// `masternode payments` does), once per database, era by era. Returns (payments fixed, blocks with
/// several payees left as they were: there are none since MN_RR on mainnet or testnet, and which
/// payee had the share added isn't recorded).
fn drop_platform_share(c: &mut Connection, mn_rr: i64) -> Result<(usize, usize)> {
    let tx = c.transaction()?;
    let top: Option<i64> = tx.query_row("SELECT max(height) FROM payments", [], |r| r.get(0))?;
    let single = "NOT EXISTS (SELECT 1 FROM payments q WHERE q.height = payments.height AND q.protx != payments.protx)";
    let mut fixed = 0;
    let mut lo = mn_rr;
    while top.is_some_and(|top| lo <= top) {
        // The era holding `lo` runs to a multiple of SUBSIDY_ERA (see platform_share).
        let hi = ((lo - 1) / SUBSIDY_ERA + 1) * SUBSIDY_ERA;
        let share = platform_share(lo);
        fixed += tx.execute(
            &format!("UPDATE payments SET amount = amount - ?3 WHERE height BETWEEN ?1 AND ?2 AND {single}"),
            params![lo, hi, share],
        )?;
        tx.execute(
            "UPDATE blocks SET mn_amount = mn_amount - ?3 WHERE height BETWEEN ?1 AND ?2
                 AND (SELECT count(*) FROM payments p WHERE p.height = blocks.height) = 1",
            params![lo, hi, share],
        )?;
        lo = hi + 1;
    }
    let skipped: usize = tx.query_row(
        "SELECT count(*) FROM (SELECT height FROM payments WHERE height >= ?1 GROUP BY height HAVING count(*) > 1)",
        [mn_rr],
        |r| r.get(0),
    )?;
    tx.pragma_update(None, "user_version", NO_PLATFORM_SHARE)?;
    tx.commit()?;
    Ok((fixed, skipped))
}

/// A panic inside a closure poisons the mutex; the connection itself is still fine to use.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let mut conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.execute_batch(SCHEMA).context("schema")?;
        migrate(&mut conn).context("migrate")?;
        Ok(Db {
            path: path.to_owned(),
            writer: Arc::new(Mutex::new(conn)),
            readers: Arc::new(Mutex::new(Vec::new())),
            read_slots: Arc::new(tokio::sync::Semaphore::new(READERS)),
        })
    }

    /// Runs `drop_platform_share` unless this database already has (`user_version`).
    pub async fn drop_platform_share(&self, mn_rr: i64) -> Result<()> {
        let fixed = self
            .write(move |c| {
                let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
                if v >= NO_PLATFORM_SHARE { Ok(None) } else { drop_platform_share(c, mn_rr).map(Some) }
            })
            .await?;
        if let Some((n, skipped)) = fixed {
            tracing::info!("{}: took the platform share out of {n} payments", self.path.display());
            if skipped > 0 {
                tracing::warn!(
                    "{}: {skipped} blocks with several payees keep their platform share",
                    self.path.display()
                );
            }
        }
        Ok(())
    }

    pub async fn write<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut Connection) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let writer = self.writer.clone();
        tokio::task::spawn_blocking(move || f(&mut lock(&writer))).await?
    }

    pub async fn read<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Connection) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let db = self.clone();
        let _slot = self.read_slots.acquire().await?;
        tokio::task::spawn_blocking(move || {
            let conn = match lock(&db.readers).pop() {
                Some(c) => c,
                None => {
                    let c = Connection::open_with_flags(&db.path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                    c.busy_timeout(std::time::Duration::from_secs(10))?;
                    c
                }
            };
            let result = f(&conn);
            let mut pool = lock(&db.readers);
            if pool.len() < READERS {
                pool.push(conn);
            }
            result
        })
        .await?
    }
}

/// Runs `sql` and collects every row through `f`.
pub fn query_all<C, T>(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    f: impl FnMut(&rusqlite::Row) -> rusqlite::Result<T>,
) -> rusqlite::Result<C>
where
    C: FromIterator<T>,
{
    let mut st = conn.prepare(sql)?;
    st.query_map(params, f)?.collect()
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

    #[test]
    fn migrates_pre_geo_nodes_table() {
        let dir = std::env::temp_dir().join(format!("dash-nodes-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.sqlite");
        let _ = std::fs::remove_file(&path);
        let geo_columns = "as_org        TEXT,\n    continent     TEXT,\n    city          TEXT,\n    lat           REAL,\n    lon           REAL\n";
        let old = SCHEMA.replace(geo_columns, "as_org        TEXT\n");
        assert_ne!(old, SCHEMA);
        Connection::open(&path).unwrap().execute_batch(&old).unwrap();
        Db::open(&path).unwrap();
        // Opening again is a no-op.
        let db = Db::open(&path).unwrap();
        lock(&db.writer)
            .execute("INSERT INTO nodes (addr, source, first_seen, city, lat, lon, continent) VALUES ('a', 's', 0, 'c', 1.5, 2.5, 'EU')", [])
            .unwrap();
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migrates_pre_readiness_database() {
        let dir = std::env::temp_dir().join(format!("dash-nodes-migrate-ready-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.sqlite");
        let _ = std::fs::remove_file(&path);
        let old = SCHEMA
            .replace(
                "    relay         INTEGER,                   -- version message relay flag (0 with blocksonly=1)\n",
                "",
            )
            .replace(
                "    platform_banned         INTEGER,         -- dashd's metaInfo.is_platform_banned (evonodes)\n",
                "",
            );
        assert!(!old.contains("relay ") && !old.contains("platform_banned"));
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(&old).unwrap();
            c.execute(
                "INSERT INTO nodes (addr, source, first_seen, services) VALUES ('1.2.3.4:9999', 'mnlist', 0, 1)",
                [],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let c = lock(&db.writer);
        // Handshakes from before the column have no relay flag: unknown, not off.
        let relay: Option<i64> = c.query_row("SELECT relay FROM nodes", [], |r| r.get(0)).unwrap();
        assert_eq!(relay, None);
        c.execute(
            "INSERT INTO masternodes (protx, type, first_seen, updated, platform_banned) VALUES ('p', 'Evo', 0, 0, 1)",
            [],
        )
        .unwrap();
        drop(c);
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migrates_pre_network_database() {
        let dir = std::env::temp_dir().join(format!("dash-nodes-migrate-net-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.sqlite");
        let _ = std::fs::remove_file(&path);
        let old = SCHEMA
            .replace("    network       TEXT,                      -- ipv4 | ipv6 | onion | i2p | cjdns\n", "")
            .replace("    core_p2p                TEXT,            -- JSON array of every core P2P address (v24 ExtNetInfo)\n", "")
            .replace("    mn_reachable INTEGER,\n    networks     TEXT ", "    mn_reachable INTEGER\n    ");
        for column in ["network       TEXT", "core_p2p                TEXT", "networks     TEXT"] {
            assert!(!old.contains(column), "{column}");
        }
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(&old).unwrap();
            c.execute_batch(
                "INSERT INTO nodes (addr, source, first_seen) VALUES ('1.2.3.4:9999', 'dashd', 0), ('[2a01::1]:9999', 'dashd', 0);
                 INSERT INTO masternodes (protx, type, service, first_seen, updated) VALUES ('p', 'Regular', '1.2.3.4:9999', 0, 0);",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let c = lock(&db.writer);
        let nets: Vec<(String, String)> =
            query_all(&c, "SELECT addr, network FROM nodes ORDER BY addr", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(nets, [("1.2.3.4:9999".into(), "ipv4".into()), ("[2a01::1]:9999".into(), "ipv6".into())]);
        // Without core_p2p yet, the masternode's addresses are its service.
        let addrs: Vec<String> = query_all(&c, "SELECT addr FROM mn_addrs", [], |r| r.get(0)).unwrap();
        assert_eq!(addrs, ["1.2.3.4:9999"]);
        c.execute("UPDATE masternodes SET core_p2p = '[\"1.2.3.4:9999\",\"[2a01::1]:9999\"]'", []).unwrap();
        let addrs: Vec<String> = query_all(&c, "SELECT addr FROM mn_addrs", [], |r| r.get(0)).unwrap();
        assert_eq!(addrs.len(), 2);
        drop(c);
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn drops_the_platform_share_once() {
        // The OP_RETURN outputs on chain: mainnet 2,563,677 (era 12), the eras before it, and testnet era 7.
        assert_eq!(platform_share(2_563_677), 46_231_323);
        assert_eq!([platform_share(2_312_640), platform_share(2_312_641)], [53_617_393, 49_787_579]);
        assert_eq!(platform_share(1_500_000), 66_966_830);

        let mut c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        // MN_RR at 2,312,000; the subsidy steps down after 2,312,640. Height 2,400,000 has two payees.
        let rows = [
            (2_311_999, "p", 300),
            (2_312_000, "p", 142_979_715),
            (2_312_640, "p", 142_990_000),
            (2_312_641, "p", 132_766_878),
            (2_400_000, "p", 100),
            (2_400_000, "q", 200),
        ];
        for (h, protx, a) in rows {
            c.execute(
                "INSERT OR IGNORE INTO blocks (height, hash, prev, time, mn_amount) VALUES (?1, '', '', 0, ?2)",
                params![h, a],
            )
            .unwrap();
            c.execute("INSERT INTO payments (height, protx, amount) VALUES (?1, ?2, ?3)", params![h, protx, a])
                .unwrap();
        }
        assert_eq!(drop_platform_share(&mut c, 2_312_000).unwrap(), (3, 1));
        let paid: Vec<(i64, i64)> =
            query_all(&c, "SELECT height, amount FROM payments ORDER BY height, protx", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(
            paid,
            [
                (2_311_999, 300),
                (2_312_000, 89_362_322),
                (2_312_640, 89_372_607),
                (2_312_641, 82_979_299),
                (2_400_000, 100),
                (2_400_000, 200)
            ]
        );
        let mn: Vec<i64> = query_all(&c, "SELECT mn_amount FROM blocks ORDER BY height", [], |r| r.get(0)).unwrap();
        assert_eq!(mn, [300, 89_362_322, 89_372_607, 82_979_299, 100]);
        let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, NO_PLATFORM_SHARE);
    }
}

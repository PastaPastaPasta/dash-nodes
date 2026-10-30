//! Follows dashd: chain status, the deterministic masternode list and per-block masternode payments.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};

use crate::addr::PeerAddr;
use crate::db::{self, get_meta, set_meta};
use crate::state::NetCtx;

/// Blocks per `masternode payments` call. Each block costs dashd a txindex lookup per input
/// (it recomputes fees), so keep calls short enough not to hog its RPC threads.
const PAYMENT_CHUNK: u32 = 100;
/// Forward indexing work per tick, so a long gap doesn't delay the masternode list refresh.
const FORWARD_PER_TICK: u32 = 2_000;
const REORG_SEARCH: u32 = 200;

#[derive(Clone, Debug, Default, Serialize)]
pub struct ChainStatus {
    pub blocks: i64,
    pub headers: i64,
    pub best_block_time: i64,
    pub verification_progress: f64,
    pub synced: bool,
    pub dashd_version: Option<String>,
    pub updated: i64,
    pub error: Option<String>,
}

pub async fn run(ctx: Arc<NetCtx>) {
    let mut mn_tip = -1;
    loop {
        match tick(&ctx, &mut mn_tip).await {
            Ok(()) => ctx.status.write().unwrap().error = None,
            Err(e) => {
                tracing::warn!(net = ctx.net.name(), "collector: {e:#}");
                ctx.status.write().unwrap().error = Some(format!("{e:#}"));
            }
        }
        tokio::time::sleep(Duration::from_secs(20)).await;
    }
}

async fn tick(ctx: &NetCtx, mn_tip: &mut i64) -> Result<()> {
    let info: Value = ctx.rpc.call("getblockchaininfo", json!([])).await?;
    let mnsync: Value = ctx.rpc.call("mnsync", json!(["status"])).await?;
    let netinfo: Value = ctx.rpc.call("getnetworkinfo", json!([])).await?;
    let blocks = info["blocks"].as_i64().unwrap_or(0);
    let synced = mnsync["IsBlockchainSynced"].as_bool().unwrap_or(false);
    {
        let mut s = ctx.status.write().unwrap();
        s.blocks = blocks;
        s.headers = info["headers"].as_i64().unwrap_or(0);
        s.best_block_time = info["mediantime"].as_i64().unwrap_or(0);
        s.verification_progress = info["verificationprogress"].as_f64().unwrap_or(0.0);
        s.synced = synced;
        s.dashd_version = netinfo["subversion"].as_str().map(str::to_owned);
        s.updated = db::now();
    }
    if !synced {
        return Ok(());
    }
    if blocks != *mn_tip {
        sync_masternodes(ctx).await.context("masternode list")?;
        *mn_tip = blocks;
        ctx.mn_loaded.store(true, std::sync::atomic::Ordering::Release);
    }
    index_forward(ctx, blocks as u32).await.context("payments")?;
    Ok(())
}

#[derive(Debug, Default, PartialEq)]
pub struct MnRow {
    pub protx: String,
    pub mn_type: String,
    pub collateral: Option<String>,
    pub collateral_address: Option<String>,
    pub operator_reward: Option<f64>,
    pub service: Option<String>,
    /// JSON array of every core P2P address, `service` first.
    pub core_p2p: Option<String>,
    pub platform_p2p: Option<String>,
    pub platform_https: Option<String>,
    pub platform_node_id: Option<String>,
    pub payout_address: Option<String>,
    pub operator_payout_address: Option<String>,
    pub owner_address: Option<String>,
    pub voting_address: Option<String>,
    pub pubkey_operator: Option<String>,
    pub registered_height: Option<i64>,
    pub last_paid_height: Option<i64>,
    pub consecutive_payments: Option<i64>,
    pub pose_penalty: Option<i64>,
    pub pose_revived_height: Option<i64>,
    pub pose_ban_height: Option<i64>,
    pub revocation_reason: Option<i64>,
}

/// Parses one `protx list registered true` entry. Tolerates both the v23 shape (`service`,
/// `payoutAddress`) and v24+ (`addresses` by purpose, `payouts`), since the deprecated fields
/// disappear unless dashd runs with -deprecatedrpc.
pub fn parse_mn(v: &Value) -> Result<MnRow> {
    let st = &v["state"];
    let s = |x: &Value| x.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    let first_addr = |purpose: &str| st["addresses"][purpose].as_array().and_then(|a| a.first()).and_then(s);
    // v24 ExtNetInfo (ProTx version 3) can list several core P2P addresses, on any network
    // (IPv4, IPv6, onion, I2P, CJDNS); the first is the primary. Normalized through PeerAddr so
    // they match the crawler's `nodes.addr` keys.
    let listed: Vec<String> =
        st["addresses"]["core_p2p"].as_array().map(|a| a.iter().filter_map(s).collect()).unwrap_or_default();
    let core_p2p: Vec<String> = if listed.is_empty() { s(&st["service"]).into_iter().collect() } else { listed }
        .into_iter()
        .filter(|a| !is_null_service(a))
        .map(|a| a.parse::<PeerAddr>().map(|p| p.to_string()).unwrap_or(a))
        .collect();
    let service = core_p2p.first().cloned();
    let platform_port = |field: &str| {
        let port = st[field].as_i64().filter(|&p| p > 0)?;
        let host = service.as_deref()?.rsplit_once(':')?.0;
        Some(format!("{host}:{port}"))
    };
    // v24 multi-payout (`payouts[].address`) and shared masternodes (`shares[].rewardAddress`,
    // `shares[].ownerAddress`, with no top-level owner) list several addresses; keep them all.
    let join = |field: &str, key: &str| {
        let addrs: Vec<String> = st[field].as_array()?.iter().filter_map(|p| s(&p[key])).collect();
        (!addrs.is_empty()).then(|| addrs.join(","))
    };
    let payout_address =
        s(&st["payoutAddress"]).or_else(|| join("payouts", "address")).or_else(|| join("shares", "rewardAddress"));
    let owner_address = s(&st["ownerAddress"]).or_else(|| join("shares", "ownerAddress"));
    let collateral = match (s(&v["collateralHash"]), v["collateralIndex"].as_i64()) {
        (Some(h), Some(i)) => Some(format!("{h}-{i}")),
        _ => None,
    };
    Ok(MnRow {
        protx: s(&v["proTxHash"]).ok_or_else(|| anyhow!("entry without proTxHash"))?,
        mn_type: s(&v["type"]).unwrap_or_else(|| "Regular".into()),
        collateral,
        collateral_address: s(&v["collateralAddress"]),
        operator_reward: v["operatorReward"].as_f64(),
        platform_p2p: first_addr("platform_p2p").or_else(|| platform_port("platformP2PPort")),
        platform_https: first_addr("platform_https").or_else(|| platform_port("platformHTTPPort")),
        service,
        core_p2p: (!core_p2p.is_empty()).then(|| json!(core_p2p).to_string()),
        platform_node_id: s(&st["platformNodeID"]),
        payout_address,
        operator_payout_address: s(&st["operatorPayoutAddress"]),
        owner_address,
        voting_address: s(&st["votingAddress"]),
        pubkey_operator: s(&st["pubKeyOperator"]),
        registered_height: st["registeredHeight"].as_i64(),
        last_paid_height: st["lastPaidHeight"].as_i64(),
        consecutive_payments: st["consecutivePayments"].as_i64(),
        pose_penalty: st["PoSePenalty"].as_i64(),
        pose_revived_height: st["PoSeRevivedHeight"].as_i64(),
        pose_ban_height: st["PoSeBanHeight"].as_i64(),
        revocation_reason: st["revocationReason"].as_i64(),
    })
}

/// Registration status from `PoSeBanHeight` (-1 or absent while the masternode isn't banned).
pub fn pose_status(ban_height: Option<i64>) -> &'static str {
    if ban_height.unwrap_or(-1) == -1 { "enabled" } else { "pose_banned" }
}

fn is_null_service(a: &str) -> bool {
    a.ends_with(":0") || a.starts_with("[::]") || a.starts_with("0.0.0.0")
}

async fn sync_masternodes(ctx: &NetCtx) -> Result<()> {
    let list: Vec<Value> = ctx.rpc.call("protx", json!(["list", "registered", true])).await?;
    let rows: Vec<MnRow> = list.iter().map(parse_mn).collect::<Result<_>>()?;
    let n = rows.len();
    ctx.db
        .write(move |conn| {
            let now = db::now();
            // A list that suddenly shrinks a lot is more likely a dashd hiccup than mass deregistration;
            // update what we got but don't mark the rest removed.
            let active: i64 = conn.query_row("SELECT count(*) FROM masternodes WHERE removed IS NULL", [], |r| r.get(0))?;
            let trust_removals = (n as i64) * 5 >= active * 4;
            if !trust_removals {
                tracing::warn!("masternode list shrank from {active} to {n}; not marking removals this round");
            }
            let tx = conn.transaction()?;
            {
                let mut up = tx.prepare_cached(
                    "INSERT INTO masternodes (protx, type, collateral, collateral_address, operator_reward, service,
                        platform_p2p, platform_https, platform_node_id, payout_address, operator_payout_address,
                        owner_address, voting_address, pubkey_operator, registered_height, last_paid_height,
                        consecutive_payments, pose_penalty, pose_revived_height, pose_ban_height, revocation_reason,
                        first_seen, updated, removed, core_p2p)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?22,NULL,?23)
                     ON CONFLICT(protx) DO UPDATE SET type=excluded.type, collateral=excluded.collateral,
                        collateral_address=excluded.collateral_address, operator_reward=excluded.operator_reward,
                        service=excluded.service, platform_p2p=excluded.platform_p2p,
                        platform_https=excluded.platform_https, platform_node_id=excluded.platform_node_id,
                        payout_address=excluded.payout_address, operator_payout_address=excluded.operator_payout_address,
                        owner_address=excluded.owner_address, voting_address=excluded.voting_address,
                        pubkey_operator=excluded.pubkey_operator, registered_height=excluded.registered_height,
                        last_paid_height=excluded.last_paid_height, consecutive_payments=excluded.consecutive_payments,
                        pose_penalty=excluded.pose_penalty, pose_revived_height=excluded.pose_revived_height,
                        pose_ban_height=excluded.pose_ban_height, revocation_reason=excluded.revocation_reason,
                        updated=excluded.updated, removed=NULL, core_p2p=excluded.core_p2p",
                )?;
                for r in &rows {
                    up.execute(params![
                        r.protx, r.mn_type, r.collateral, r.collateral_address, r.operator_reward, r.service,
                        r.platform_p2p, r.platform_https, r.platform_node_id, r.payout_address,
                        r.operator_payout_address, r.owner_address, r.voting_address, r.pubkey_operator,
                        r.registered_height, r.last_paid_height, r.consecutive_payments, r.pose_penalty,
                        r.pose_revived_height, r.pose_ban_height, r.revocation_reason, now, r.core_p2p,
                    ])?;
                }
            }
            if trust_removals {
                tx.execute("UPDATE masternodes SET removed = ?1 WHERE removed IS NULL AND updated < ?1", [now])?;
            }
            tx.commit()?;
            Ok(())
        })
        .await?;
    tracing::debug!(net = ctx.net.name(), "masternode list: {n} registered");
    Ok(())
}

// ---- payments ------------------------------------------------------------------------------
//
// Indexed range is [pay_lo, pay_hi]. The collector extends it upward as blocks arrive; the
// backfill task extends it downward to the DIP3 enforcement height (or --payments-floor). Both
// hold `ctx.pay_lock`, so the range and the tables only ever change one batch at a time, and every
// batch is checked to link onto its stored neighbours, so the stored blocks always form a chain.

fn range(conn: &Connection) -> Result<Option<(u32, u32)>> {
    let lo = get_meta(conn, "pay_lo")?.and_then(|v| v.parse().ok());
    let hi = get_meta(conn, "pay_hi")?.and_then(|v| v.parse().ok());
    Ok(lo.zip(hi))
}

pub async fn payments_range(ctx: &NetCtx) -> Result<Option<(u32, u32)>> {
    ctx.db.read(range).await
}

/// Which end of the indexed range a batch extends.
#[derive(Clone, Copy)]
enum Extend {
    First,
    Up,
    Down,
}

#[derive(Debug)]
struct ChainMismatch;

impl std::fmt::Display for ChainMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("batch doesn't link onto stored blocks (reorg)")
    }
}

impl std::error::Error for ChainMismatch {}

async fn index_forward(ctx: &NetCtx, tip: u32) -> Result<()> {
    let _guard = ctx.pay_lock.lock().await;
    let Some((lo, hi)) = payments_range(ctx).await? else {
        // First run: start with just the tip; backfill takes it from there.
        return index_blocks(ctx, tip, tip, Extend::First).await;
    };
    // A batch that fails to link (ChainMismatch) means the stored top is off the active chain;
    // the next tick's unwind finds and drops it.
    let mut hi = unwind_reorg(ctx, lo, hi, tip).await?;
    let end = tip.min(hi + FORWARD_PER_TICK);
    while hi < end {
        let to = end.min(hi + PAYMENT_CHUNK);
        index_blocks(ctx, hi + 1, to, Extend::Up).await?;
        hi = to;
    }
    Ok(())
}

/// Drops stored blocks that are no longer on the active chain (or above the tip), from the top
/// down. Returns the new top of the range, which falls below `lo` if the whole range was stale.
async fn unwind_reorg(ctx: &NetCtx, lo: u32, hi: u32, tip: u32) -> Result<u32> {
    let mut h = hi.min(tip);
    while h >= lo && hi - h < REORG_SEARCH {
        let stored: Option<String> = ctx
            .db
            .read(move |c| {
                Ok(c.query_row("SELECT hash FROM blocks WHERE height = ?1", [h], |r| r.get(0)).optional()?)
            })
            .await?;
        let actual: String = ctx.rpc.call("getblockhash", json!([h])).await?;
        if stored.as_deref() == Some(actual.as_str()) || h == 0 {
            break;
        }
        h -= 1;
    }
    if h < hi {
        tracing::info!(net = ctx.net.name(), "reorg: unwinding payments above {h}");
        ctx.db
            .write(move |c| {
                let tx = c.transaction()?;
                tx.execute("DELETE FROM payments WHERE height > ?1", [h])?;
                tx.execute("DELETE FROM blocks WHERE height > ?1", [h])?;
                set_meta(&tx, "pay_hi", &h.to_string())?;
                if h < lo {
                    set_meta(&tx, "pay_lo", &(h + 1).to_string())?;
                }
                tx.commit()?;
                Ok(())
            })
            .await?;
    }
    Ok(h)
}

/// Fetches payments for heights [from, to] (inclusive), checks the batch is a chain that links onto
/// whatever is stored at from-1 and to+1, and stores it together with the new range bound.
async fn index_blocks(ctx: &NetCtx, from: u32, to: u32, extend: Extend) -> Result<()> {
    let top: String = ctx.rpc.call("getblockhash", json!([to])).await?;
    let count = i64::from(to - from + 1);
    let blocks: Vec<Value> = ctx.rpc.call("masternode", json!(["payments", top, -count])).await?;
    if blocks.len() != count as usize {
        bail!("masternode payments returned {} blocks, expected {count}", blocks.len());
    }
    let calls: Vec<(&str, Value)> =
        blocks.iter().map(|b| ("getblockheader", json!([b["blockhash"].clone(), true]))).collect();
    let headers = ctx.rpc.batch(&calls).await?;

    struct Block {
        height: i64,
        hash: String,
        prev: String,
        time: i64,
        amount: i64,
        payees: Vec<(String, i64)>,
    }
    let mut rows: Vec<Block> = Vec::with_capacity(blocks.len());
    for (b, h) in blocks.iter().zip(&headers) {
        let payees = b["masternodes"]
            .as_array()
            .map(|mns| {
                mns.iter()
                    .filter_map(|m| {
                        let protx =
                            m["proTxHash"].as_str().filter(|p| !p.is_empty() && !p.bytes().all(|c| c == b'0'))?;
                        Some((protx.to_owned(), m["amount"].as_i64()?))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let block = Block {
            height: b["height"].as_i64().ok_or_else(|| anyhow!("payments: no height"))?,
            hash: b["blockhash"].as_str().unwrap_or_default().to_owned(),
            prev: h["previousblockhash"].as_str().unwrap_or_default().to_owned(),
            time: h["time"].as_i64().unwrap_or(0),
            amount: b["amount"].as_i64().unwrap_or(0),
            payees,
        };
        if let Some(below) = rows.last()
            && (block.prev != below.hash || block.height != below.height + 1)
        {
            return Err(ChainMismatch.into());
        }
        rows.push(block);
    }
    ctx.db
        .write(move |c| {
            let tx = c.transaction()?;
            let stored = |h: i64| -> rusqlite::Result<Option<(String, String)>> {
                tx.query_row("SELECT hash, prev FROM blocks WHERE height = ?1", [h], |r| Ok((r.get(0)?, r.get(1)?)))
                    .optional()
            };
            let (first, last) = (&rows[0], &rows[rows.len() - 1]);
            if let Some((hash, _)) = stored(first.height - 1)?
                && hash != first.prev
            {
                return Err(ChainMismatch.into());
            }
            if let Some((_, prev)) = stored(last.height + 1)?
                && prev != last.hash
            {
                return Err(ChainMismatch.into());
            }
            {
                let mut ins_block = tx.prepare_cached(
                    "INSERT OR REPLACE INTO blocks (height, hash, prev, time, mn_amount) VALUES (?1, ?2, ?3, ?4, ?5)",
                )?;
                let mut del = tx.prepare_cached("DELETE FROM payments WHERE height = ?1")?;
                let mut ins_pay =
                    tx.prepare_cached("INSERT OR REPLACE INTO payments (height, protx, amount) VALUES (?1, ?2, ?3)")?;
                for b in &rows {
                    ins_block.execute(params![b.height, b.hash, b.prev, b.time, b.amount])?;
                    del.execute([b.height])?;
                    for (protx, amount) in &b.payees {
                        ins_pay.execute(params![b.height, protx, amount])?;
                    }
                }
            }
            match extend {
                Extend::First => {
                    set_meta(&tx, "pay_lo", &from.to_string())?;
                    set_meta(&tx, "pay_hi", &to.to_string())?;
                }
                Extend::Up => set_meta(&tx, "pay_hi", &to.to_string())?,
                Extend::Down => set_meta(&tx, "pay_lo", &from.to_string())?,
            }
            tx.commit()?;
            Ok(())
        })
        .await
}

/// Walks the payments index down to `floor`, newest first, so recent history is there early.
pub async fn backfill(ctx: Arc<NetCtx>, floor: u32) {
    loop {
        if !ctx.status.read().unwrap().synced {
            tokio::time::sleep(Duration::from_secs(30)).await;
            continue;
        }
        let step: Result<bool> = async {
            let _guard = ctx.pay_lock.lock().await;
            let Some((lo, _)) = payments_range(&ctx).await? else {
                return Ok(false);
            };
            if lo <= floor {
                return Ok(true);
            }
            let from = floor.max(lo.saturating_sub(PAYMENT_CHUNK));
            index_blocks(&ctx, from, lo - 1, Extend::Down).await?;
            Ok(false)
        }
        .await;
        match step {
            Ok(true) => {
                tracing::info!(net = ctx.net.name(), "payments backfill complete down to {floor}");
                return;
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(net = ctx.net.name(), "backfill: {e:#}");
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        }
        // Yield dashd to the collector and anything else using it.
        tokio::time::sleep(Duration::from_millis(ctx.backfill_pause_ms)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v23_entry() {
        let v = json!({
            "type": "Evo", "proTxHash": "aa", "collateralHash": "cc", "collateralIndex": 1,
            "collateralAddress": "Xc", "operatorReward": 0.0,
            "state": {
                "version": 2, "service": "1.2.3.4:9999", "registeredHeight": 10, "lastPaidHeight": 20,
                "consecutivePayments": 0, "PoSePenalty": 0, "PoSeRevivedHeight": -1, "PoSeBanHeight": -1,
                "revocationReason": 0, "ownerAddress": "Xo", "votingAddress": "Xv",
                "platformNodeID": "ff", "platformP2PPort": 26656, "platformHTTPPort": 443,
                "payoutAddress": "Xp", "pubKeyOperator": "8f"
            }
        });
        let r = parse_mn(&v).unwrap();
        assert_eq!(r.service.as_deref(), Some("1.2.3.4:9999"));
        assert_eq!(r.platform_p2p.as_deref(), Some("1.2.3.4:26656"));
        assert_eq!(r.platform_https.as_deref(), Some("1.2.3.4:443"));
        assert_eq!(r.payout_address.as_deref(), Some("Xp"));
        assert_eq!(r.collateral.as_deref(), Some("cc-1"));
        assert_eq!(r.pose_ban_height, Some(-1));
    }

    #[test]
    fn parses_v24_entry() {
        let v = json!({
            "type": "Regular", "proTxHash": "bb",
            "state": {
                "addresses": {"core_p2p": ["5.6.7.8:9999"]},
                "payouts": [{"address": "Xa", "reward": 50}, {"address": "Xb", "reward": 50}],
                "PoSeBanHeight": 1234
            }
        });
        let r = parse_mn(&v).unwrap();
        assert_eq!(r.service.as_deref(), Some("5.6.7.8:9999"));
        assert_eq!(r.core_p2p.as_deref(), Some(r#"["5.6.7.8:9999"]"#));
        assert_eq!(r.payout_address.as_deref(), Some("Xa,Xb"));
        assert_eq!(r.pose_ban_height, Some(1234));
    }

    #[test]
    fn keeps_every_core_p2p_address() {
        // ProTx version 3 (ExtNetInfo): several addresses on different networks, the first primary.
        let onion = "pg6mmjiyjmcrsslvykfwnntlaru7p5svn6y2ymmju6nubxndf4pscryd.onion:9999";
        let v = json!({
            "type": "Evo", "proTxHash": "dd",
            "state": {"version": 3, "addresses": {
                "core_p2p": ["[2a01:4f8:0:0::1]:19999", onion.to_uppercase(), "1.2.3.4:19999"],
                "platform_https": ["evo.example.com:443"]
            }}
        });
        let r = parse_mn(&v).unwrap();
        assert_eq!(r.service.as_deref(), Some("[2a01:4f8::1]:19999"));
        let all: Vec<String> = serde_json::from_str(r.core_p2p.as_deref().unwrap()).unwrap();
        assert_eq!(all, ["[2a01:4f8::1]:19999", onion, "1.2.3.4:19999"]);
        assert_eq!(r.platform_https.as_deref(), Some("evo.example.com:443"));
    }

    #[test]
    fn null_service_is_none() {
        let v = json!({"proTxHash": "cc", "state": {"service": "[::]:0"}});
        let r = parse_mn(&v).unwrap();
        assert_eq!(r.service, None);
        assert_eq!(r.core_p2p, None);
    }
}

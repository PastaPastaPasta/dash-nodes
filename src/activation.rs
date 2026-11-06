//! Dash Core 24's activation: where the `v24` deployment stands and when it can lock in.
//!
//! `v24` is a BIP9-style deployment with two gates (Dash Core v24.0.0, `src/versionbits.cpp`,
//! `src/evo/mnhftx.cpp`, `src/llmq/ehf_signals.cpp`):
//!
//! 1. **The masternode signal (EHF, DIP-0023).** Once the median time past reaches the start time,
//!    masternodes running 24 sign a request for the deployment's bit in one `llmq_mnhf` quorum
//!    (llmq_400_85 on mainnet: 340 of its 400 members must sign). A miner puts the signature in a
//!    block; that height is `ehf_height`. Until then the deployment stays DEFINED, whatever the
//!    clock says.
//! 2. **Miners (versionbits).** From the first window (`nWindowSize` blocks, aligned to heights that
//!    are multiples of it) whose previous block is at or past both the start time (median time past)
//!    and `ehf_height`, the deployment is STARTED and blocks signal by setting the bit, with the top
//!    three version bits `001`. A window with at least the threshold's signalling blocks locks it in;
//!    the next window it is LOCKED_IN and the one after it ACTIVE. The threshold starts at
//!    `nThresholdStart` and falls each failed window ("attempt") as
//!    `start - attempt² × window / 100 / falloff` (integer arithmetic), never below `nThresholdMin`.
//!    A window ending at or past the timeout (median time past) without locking in fails it.
//!
//! We read every block header since a little before the window that matters, work the states out
//! ourselves with Core's rules, and check them against our dashd's `getdeploymentinfo`. Estimated
//! dates use the recent average block interval and say so.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::db::{self, get_meta, query_all, set_meta};
use crate::network::Network;
use crate::state::NetCtx;

/// Core's `BIP9Deployment::NO_TIMEOUT`.
const NO_TIMEOUT: i64 = i64::MAX;
/// Version bits deployments (`VERSIONBITS_TOP_MASK`, `VERSIONBITS_TOP_BITS`, src/versionbits.h).
const TOP_MASK: u32 = 0xE000_0000;
const TOP_BITS: u32 = 0x2000_0000;

/// One deployment's consensus parameters, as released (src/chainparams.cpp at v24.0.0).
#[derive(Clone, Copy, Debug)]
pub struct Deployment {
    pub name: &'static str,
    pub bit: u32,
    pub start: i64,
    pub timeout: i64,
    pub window: i64,
    pub threshold_start: i64,
    pub threshold_min: i64,
    pub falloff: i64,
    pub min_activation_height: i64,
    /// Whether this release's masternodes sign its EHF signal: 24.0.0 skips `evo_shares`
    /// (src/llmq/ehf_signals.cpp), so that one can't start until a later release signs it.
    pub signed: bool,
}

/// The quorum that signs EHF requests (`llmqTypeMnhf`, src/chainparams.cpp, src/llmq/params.h).
#[derive(Clone, Copy, Debug)]
pub struct MnhfQuorum {
    pub llmq_type: u8,
    pub name: &'static str,
    pub size: i64,
    pub threshold: i64,
}

const MAINNET: [Deployment; 2] = [
    Deployment {
        name: "v24",
        bit: 13,
        start: 1_794_182_400,   // 2026-11-09 00:00:00 UTC
        timeout: 1_857_340_800, // 2028-11-09 00:00:00 UTC
        window: 4032,
        threshold_start: 3226,
        threshold_min: 2420,
        falloff: 5,
        min_activation_height: 0,
        signed: true,
    },
    Deployment {
        name: "evo_shares",
        bit: 14,
        start: 1_794_182_400,
        timeout: 1_857_340_800,
        window: 4032,
        threshold_start: 3226,
        threshold_min: 2420,
        falloff: 5,
        min_activation_height: 0,
        signed: false,
    },
];

const TESTNET: [Deployment; 2] = [
    Deployment {
        name: "v24",
        bit: 13,
        start: 1_790_208_000, // 2026-09-24 00:00:00 UTC
        timeout: NO_TIMEOUT,
        window: 100,
        threshold_start: 80,
        threshold_min: 60,
        falloff: 5,
        min_activation_height: 0,
        signed: true,
    },
    Deployment {
        name: "evo_shares",
        bit: 14,
        start: 1_790_208_000,
        timeout: NO_TIMEOUT,
        window: 100,
        threshold_start: 80,
        threshold_min: 60,
        falloff: 5,
        min_activation_height: 0,
        signed: false,
    },
];

pub fn deployments(net: Network) -> &'static [Deployment] {
    match net {
        Network::Mainnet => &MAINNET,
        Network::Testnet => &TESTNET,
    }
}

pub fn mnhf_quorum(net: Network) -> MnhfQuorum {
    match net {
        Network::Mainnet => MnhfQuorum { llmq_type: 3, name: "llmq_400_85", size: 400, threshold: 340 },
        Network::Testnet => MnhfQuorum { llmq_type: 1, name: "llmq_50_60", size: 50, threshold: 30 },
    }
}

/// Core's `VersionBitsConditionChecker::Threshold`: signalling blocks a window needs on `attempt`
/// (0 for the first STARTED window).
pub fn threshold(d: &Deployment, attempt: i64) -> i64 {
    if d.threshold_min == 0 || d.falloff == 0 {
        return d.threshold_start;
    }
    (d.threshold_start - attempt * attempt * d.window / 100 / d.falloff).max(d.threshold_min)
}

/// Core's `Condition`: does a block with this `nVersion` signal for `bit`?
pub fn signals(version: i64, bit: u32) -> bool {
    let v = version as i32 as u32;
    v & TOP_MASK == TOP_BITS && v & (1 << bit) != 0
}

/// The EHF request id the quorum signs for `bit`: `SerializeHash(make_pair("mnhf", int64_t{bit}))`
/// (MNHFTxPayload::GetRequestId), in RPC (reversed) hex.
pub fn request_id(bit: u32) -> String {
    let mut buf = vec![4u8];
    buf.extend_from_slice(b"mnhf");
    buf.extend_from_slice(&i64::from(bit).to_le_bytes());
    let mut h: [u8; 32] = Sha256::digest(Sha256::digest(&buf)).into();
    h.reverse();
    h.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Defined,
    Started,
    LockedIn,
    Active,
    Failed,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Defined => "defined",
            State::Started => "started",
            State::LockedIn => "locked_in",
            State::Active => "active",
            State::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<State> {
        Some(match s {
            "defined" => State::Defined,
            "started" => State::Started,
            "locked_in" => State::LockedIn,
            "active" => State::Active,
            "failed" => State::Failed,
            _ => return None,
        })
    }
}

/// One window's blocks as far as the chain goes.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    /// Height of its first block (a multiple of the deployment's window size).
    pub start: i64,
    /// Blocks of it mined so far (the window size once it's complete).
    pub mined: i64,
    /// Of those, blocks that signal.
    pub count: i64,
    /// Median time past of its last block, once it's complete.
    pub mtp_end: Option<i64>,
}

/// A window's state and, while STARTED, its attempt and threshold.
#[derive(Clone, Debug, PartialEq)]
pub struct Judged {
    pub window: Window,
    pub state: State,
    /// Height of the first block of the run of windows in this state (Core's `StateSinceHeight`).
    pub since: i64,
    pub attempt: Option<i64>,
    pub threshold: Option<i64>,
}

/// Core's `GetStateFor`, window by window. `seed` is the state of `windows[0]` and the height it has
/// held since; `signal` is the EHF signal's height (None: not mined). If the last window is complete,
/// the one after it (with nothing mined yet) is appended, so the result always ends with the window
/// the next block belongs to.
pub fn walk(d: &Deployment, windows: &[Window], seed: (State, i64), signal: Option<i64>) -> Vec<Judged> {
    let (mut state, mut since) = seed;
    let mut out = Vec::with_capacity(windows.len() + 1);
    for w in windows {
        let (attempt, threshold) = if state == State::Started {
            let a = (w.start - since) / d.window;
            (Some(a), Some(threshold(d, a)))
        } else {
            (None, None)
        };
        out.push(Judged { window: w.clone(), state, since, attempt, threshold });
        if w.mined < d.window {
            return out;
        }
        // The transition for the next window, judged at this window's last block.
        let end = w.start + d.window - 1;
        let mtp = w.mtp_end.unwrap_or(i64::MIN);
        let next = match state {
            State::Defined if mtp >= d.start && signal.is_some_and(|s| end >= s) => State::Started,
            State::Started if threshold.is_some_and(|t| w.count >= t) => State::LockedIn,
            State::Started if mtp >= d.timeout => State::Failed,
            State::LockedIn if end + 1 >= d.min_activation_height => State::Active,
            s => s,
        };
        if next != state {
            state = next;
            since = end + 1;
        }
    }
    if let Some(last) = windows.last() {
        let start = last.start + d.window;
        let (attempt, threshold) = if state == State::Started {
            let a = (start - since) / d.window;
            (Some(a), Some(threshold(d, a)))
        } else {
            (None, None)
        };
        out.push(Judged {
            window: Window { start, mined: 0, count: 0, mtp_end: None },
            state,
            since,
            attempt,
            threshold,
        });
    }
    out
}

/// A stored block header.
#[derive(Clone, Copy, Debug)]
pub struct Header {
    pub height: i64,
    pub time: i64,
    pub mediantime: i64,
    pub version: i64,
}

/// Groups headers (ascending, contiguous) into windows of `d`, counting its signals.
pub fn windows_of(d: &Deployment, headers: &[Header]) -> Vec<Window> {
    let mut out: Vec<Window> = Vec::new();
    for h in headers {
        let start = h.height - h.height.rem_euclid(d.window);
        if out.last().is_none_or(|w| w.start != start) {
            out.push(Window { start, mined: 0, count: 0, mtp_end: None });
        }
        let w = out.last_mut().unwrap();
        w.mined += 1;
        w.count += i64::from(signals(h.version, d.bit));
        if h.height == start + d.window - 1 {
            w.mtp_end = Some(h.mediantime);
        }
    }
    // A first window that starts before our oldest block can't be counted: drop it.
    if out.first().is_some_and(|w| headers.first().is_some_and(|h| h.height != w.start)) {
        out.remove(0);
    }
    out
}

// ---- collecting ----

const TICK: Duration = Duration::from_secs(30);
/// Recent blocks re-checked for a reorg on each tick.
const REORG_DEPTH: i64 = 48;
/// Headers fetched per RPC batch.
const CHUNK: i64 = 250;
/// Never track more than this many blocks back from the tip.
const MAX_HISTORY: i64 = 60_000;
/// Blocks the average interval is measured over.
const PACE_BLOCKS: i64 = 2016;
/// Recent blocks the "signalling lately" figure covers.
const RECENT_BLOCKS: i64 = 576;
/// How often the signing quorum's members are looked up again.
const GATE_EVERY: i64 = 600;
/// Windows listed in `windows` at most (the newest).
const MAX_WINDOWS: usize = 200;

/// Where a reading comes from: one network's dashd, and the database the headers are kept in.
pub struct Source<'a> {
    pub net: Network,
    pub rpc: &'a crate::rpc::Rpc,
    pub db: &'a crate::db::Db,
    pub synced: bool,
    pub dashd: Option<String>,
}

/// One reading, for the `activation` command.
pub async fn once(src: &Source<'_>) -> Result<Value> {
    tick(src, &mut Memo::default()).await
}

/// Per deployment, the state at the first tracked block and the height it holds since.
type Seeds = HashMap<String, (State, i64)>;

/// What the collector keeps between ticks.
#[derive(Default)]
struct Memo {
    /// `getdeploymentinfo` at the first tracked block, per deployment: (state, since).
    seed: Option<(i64, Seeds)>,
    /// The signing-quorum view per deployment, and when it was read.
    gate: HashMap<String, (i64, Value)>,
    last_error: Option<(i64, String)>,
}

pub async fn run(ctx: Arc<NetCtx>) {
    let mut memo = Memo::default();
    loop {
        let (synced, dashd) = {
            let s = ctx.status.read().unwrap();
            (s.synced, s.dashd_version.clone())
        };
        let src = Source { net: ctx.net, rpc: &ctx.rpc, db: &ctx.db, synced, dashd };
        match tick(&src, &mut memo).await {
            Ok(doc) => {
                memo.last_error = None;
                *ctx.activation.write().unwrap() = Some(Arc::new(doc));
            }
            Err(e) => {
                tracing::warn!(net = ctx.net.name(), "activation: {e:#}");
                let since = memo.last_error.as_ref().map_or_else(db::now, |(t, _)| *t);
                memo.last_error = Some((since, format!("{e:#}")));
                // Keep serving the last good reading, saying it's stale and why.
                let mut slot = ctx.activation.write().unwrap();
                if let Some(doc) = slot.as_ref() {
                    let mut doc = (**doc).clone();
                    doc["error"] = json!({"message": format!("{e:#}"), "since": since});
                    *slot = Some(Arc::new(doc));
                }
            }
        }
        tokio::time::sleep(TICK).await;
    }
}

async fn tick(src: &Source<'_>, memo: &mut Memo) -> Result<Value> {
    if !src.synced {
        bail!("dashd isn't synced yet");
    }
    let info: Value = src.rpc.call("getdeploymentinfo", json!([])).await?;
    let tip = info["height"].as_i64().ok_or_else(|| anyhow!("getdeploymentinfo: no height"))?;
    let rpc_deps = info["deployments"].clone();
    let deps = deployments(src.net);

    // Where tracking starts: two windows back, or the window before a mined EHF signal.
    let from = match src.db.read(|c| get_meta(c, "activation_from")).await?.and_then(|s| s.parse::<i64>().ok()) {
        Some(f) => f,
        None => {
            let w = deps.iter().map(|d| d.window).max().unwrap_or(1);
            let mut from = (tip / w - 2) * w;
            for d in deps {
                if let Some(h) = rpc_deps[d.name]["bip9"]["ehf_height"].as_i64() {
                    from = from.min((h / d.window - 1) * d.window);
                }
            }
            let from = from.max(tip - MAX_HISTORY).max(0);
            let from = from - from.rem_euclid(w);
            src.db.write(move |c| set_meta(c, "activation_from", &from.to_string())).await?;
            from
        }
    };
    sync_headers(src, from, tip).await?;

    if memo.seed.as_ref().is_none_or(|(f, _)| *f != from) {
        let hash: String = src.rpc.call("getblockhash", json!([from])).await?;
        let at: Value = src.rpc.call("getdeploymentinfo", json!([hash])).await?;
        let mut seeds = HashMap::new();
        for d in deps {
            let b = &at["deployments"][d.name]["bip9"];
            let state = b["status"].as_str().and_then(State::parse).unwrap_or(State::Defined);
            seeds.insert(d.name.to_owned(), (state, b["since"].as_i64().unwrap_or(0)));
        }
        memo.seed = Some((from, seeds));
    }
    let seeds = memo.seed.as_ref().unwrap().1.clone();

    // The signing quorum, for deployments whose signal this release signs and nobody has mined yet.
    let now = db::now();
    for d in deps {
        let mined = rpc_deps[d.name]["bip9"]["ehf_height"].as_i64().is_some();
        if !d.signed || mined {
            memo.gate.remove(d.name);
            continue;
        }
        if memo.gate.get(d.name).is_none_or(|(t, _)| now - t >= GATE_EVERY) {
            let view = match signing_quorum(src, d).await {
                Ok(v) => v,
                Err(e) => json!({"error": format!("{e:#}")}),
            };
            memo.gate.insert(d.name.to_owned(), (now, view));
        }
    }

    let net = src.net;
    let gate = memo.gate.clone();
    let dashd = src.dashd.clone();
    src.db
        .read(move |c| {
            let headers = load_headers(c, from)?;
            build(net, &headers, tip, &rpc_deps, &seeds, &gate, dashd, now)
        })
        .await
}

/// Brings `activation_headers` to the tip: drops blocks a reorg replaced, then fetches the rest.
async fn sync_headers(src: &Source<'_>, from: i64, tip: i64) -> Result<()> {
    let top: Option<i64> =
        src.db.read(|c| Ok(c.query_row("SELECT max(height) FROM activation_headers", [], |r| r.get(0))?)).await?;
    let mut next = from;
    if let Some(top) = top {
        // Compare the newest stored hashes with the chain's; drop everything from the first that differs.
        let lo = (top - REORG_DEPTH + 1).max(from);
        let stored: Vec<(i64, String)> = src
            .db
            .read(move |c| {
                Ok(query_all(
                    c,
                    "SELECT height, hash FROM activation_headers WHERE height >= ?1 ORDER BY height",
                    [lo],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .await?;
        let calls: Vec<(&str, Value)> =
            stored.iter().filter(|(h, _)| *h <= tip).map(|(h, _)| ("getblockhash", json!([h]))).collect();
        let live = src.rpc.batch(&calls).await?;
        let mut bad = stored.iter().zip(&live).find(|((_, s), l)| l.as_str() != Some(s)).map(|((h, _), _)| *h);
        // Stored above a tip that moved back: those go too.
        if bad.is_none() && top > tip {
            bad = Some(tip + 1);
        }
        if let Some(h) = bad {
            // Past the depth we compare: drop that whole span and look further back next tick.
            let h = if h == lo && lo > from { (lo - REORG_DEPTH).max(from) } else { h };
            tracing::info!(net = src.net.name(), "activation: reorg, dropping headers from {h}");
            src.db.write(move |c| Ok(c.execute("DELETE FROM activation_headers WHERE height >= ?1", [h])?)).await?;
            next = h;
        } else {
            next = top + 1;
        }
        // Stored history must start at `from` and have no holes.
        let (lowest, n): (Option<i64>, i64) = src
            .db
            .read(|c| {
                Ok(c.query_row("SELECT min(height), count(*) FROM activation_headers", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?)
            })
            .await?;
        if lowest.is_some_and(|l| l != from) || lowest.is_some_and(|l| n != next - l) {
            src.db.write(|c| Ok(c.execute("DELETE FROM activation_headers", [])?)).await?;
            next = from;
        }
    }
    while next <= tip {
        let to = (next + CHUNK - 1).min(tip);
        let hashes = src.rpc.batch(&(next..=to).map(|h| ("getblockhash", json!([h]))).collect::<Vec<_>>()).await?;
        let calls: Vec<(&str, Value)> = hashes.iter().map(|h| ("getblockheader", json!([h, true]))).collect();
        let headers = src.rpc.batch(&calls).await?;
        let mut rows = Vec::with_capacity(headers.len());
        for (i, h) in headers.iter().enumerate() {
            let height = h["height"].as_i64().context("header height")?;
            if height != next + i as i64 {
                bail!("header {height} where {} was expected", next + i as i64);
            }
            rows.push((
                height,
                h["hash"].as_str().context("header hash")?.to_owned(),
                h["previousblockhash"].as_str().unwrap_or_default().to_owned(),
                h["time"].as_i64().context("header time")?,
                h["mediantime"].as_i64().context("header mediantime")?,
                h["version"].as_i64().context("header version")?,
            ));
        }
        src.db
            .write(move |c| {
                let tx = c.transaction()?;
                {
                    // A block whose parent isn't the stored one means the chain moved under us: stop and
                    // let the next tick's reorg check sort it out.
                    let prev: Option<String> = tx
                        .query_row("SELECT hash FROM activation_headers WHERE height = ?1", [rows[0].0 - 1], |r| r.get(0))
                        .ok();
                    if prev.is_some_and(|p| p != rows[0].2) {
                        bail!("chain moved while reading headers");
                    }
                    let mut st = tx.prepare(
                        "INSERT OR REPLACE INTO activation_headers (height, hash, time, mediantime, version) VALUES (?1, ?2, ?3, ?4, ?5)",
                    )?;
                    for w in rows.windows(2) {
                        if w[1].2 != w[0].1 {
                            bail!("chain moved while reading headers");
                        }
                    }
                    for (height, hash, _, time, mtp, version) in &rows {
                        st.execute(params![height, hash, time, mtp, version])?;
                    }
                }
                tx.commit()?;
                Ok(())
            })
            .await?;
        next = to + 1;
    }
    Ok(())
}

fn load_headers(c: &Connection, from: i64) -> Result<Vec<Header>> {
    Ok(query_all(
        c,
        "SELECT height, time, mediantime, version FROM activation_headers WHERE height >= ?1 ORDER BY height",
        [from],
        |r| Ok(Header { height: r.get(0)?, time: r.get(1)?, mediantime: r.get(2)?, version: r.get(3)? }),
    )?)
}

/// The quorum that would sign `d`'s EHF request now, and the active ones, with how many of their valid
/// members told our crawler (in the past week) that they run protocol MIN_MASTERNODE_PROTO_VERSION.
async fn signing_quorum(src: &Source<'_>, d: &Deployment) -> Result<Value> {
    let q = mnhf_quorum(src.net);
    let id = request_id(d.bit);
    let selected: Value = src.rpc.call("quorum", json!(["selectquorum", q.llmq_type, id])).await?;
    let selected = selected["quorumHash"].as_str().ok_or_else(|| anyhow!("selectquorum: no quorumHash"))?.to_owned();
    let list: Value = src.rpc.call("quorum", json!(["list"])).await?;
    let mut hashes: Vec<String> = list[q.name]
        .as_array()
        .map(|a| a.iter().filter_map(|h| h.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    if !hashes.contains(&selected) {
        hashes.insert(0, selected.clone());
    }
    let calls: Vec<(&str, Value)> = hashes.iter().map(|h| ("quorum", json!(["info", q.llmq_type, h]))).collect();
    let infos = src.rpc.batch(&calls).await?;
    let protx: Vec<Vec<(String, bool)>> = infos
        .iter()
        .map(|i| {
            i["members"]
                .as_array()
                .map(|m| {
                    m.iter()
                        .filter_map(|x| {
                            Some((x["proTxHash"].as_str()?.to_owned(), x["valid"].as_bool().unwrap_or(false)))
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
        .collect();
    let now = db::now();
    let versions: HashMap<String, Option<i64>> = src
        .db
        .read(move |c| {
            let rows: Vec<(String, Option<i64>, Option<i64>)> = query_all(
                c,
                "SELECT m.protx, n.protocol, n.last_success FROM masternodes m LEFT JOIN nodes n ON n.addr = m.service
                 WHERE m.removed IS NULL",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            Ok(rows
                .into_iter()
                .map(|(p, proto, seen)| {
                    let recent = seen.is_some_and(|t| now - t < crate::crawler::ACTIVE_WINDOW);
                    (p, if recent { proto } else { None })
                })
                .collect())
        })
        .await?;
    let min = crate::readiness::core::MIN_MASTERNODE_PROTO_VERSION;
    let views: Vec<Value> = hashes
        .iter()
        .zip(&infos)
        .zip(&protx)
        .map(|((hash, info), members)| {
            let (mut valid, mut current, mut older, mut unknown) = (0, 0, 0, 0);
            for (p, ok) in members {
                if !ok {
                    continue;
                }
                valid += 1;
                match versions.get(p).copied().flatten() {
                    Some(v) if v >= min => current += 1,
                    Some(_) => older += 1,
                    None => unknown += 1,
                }
            }
            json!({
                "hash": hash, "height": info["height"], "selected": *hash == selected,
                "members": members.len(), "valid": valid, "current": current, "older": older, "unknown": unknown,
            })
        })
        .collect();
    Ok(json!({
        "llmq": q.name, "size": q.size, "threshold": q.threshold, "request_id": id, "min_protocol": min,
        "quorums": views, "checked": now,
    }))
}

// ---- the document ----

/// A height with its time: the block's own if it's mined, else an estimate from the average pace.
fn at(height: i64, headers: &[Header], tip: &Header, pace: f64) -> Value {
    if height <= tip.height {
        let first = headers.first().map_or(i64::MAX, |h| h.height);
        let time = (height >= first).then(|| headers[(height - first) as usize].time);
        return json!({"height": height, "time": time, "estimated": false});
    }
    let t = tip.time as f64 + (height - tip.height) as f64 * pace;
    json!({"height": height, "time": t.round() as i64, "estimated": true})
}

/// Seconds per block over the last PACE_BLOCKS blocks (150 if we have too few).
fn pace(headers: &[Header]) -> f64 {
    let n = headers.len().min(PACE_BLOCKS as usize + 1);
    if n < 2 {
        return 150.0;
    }
    let (a, b) = (&headers[headers.len() - n], &headers[headers.len() - 1]);
    ((b.time - a.time) as f64 / (b.height - a.height) as f64).max(1.0)
}

#[allow(clippy::too_many_arguments)]
fn build(
    net: Network,
    headers: &[Header],
    tip_height: i64,
    rpc: &Value,
    seeds: &HashMap<String, (State, i64)>,
    gate: &HashMap<String, (i64, Value)>,
    dashd: Option<String>,
    now: i64,
) -> Result<Value> {
    let tip = *headers.last().ok_or_else(|| anyhow!("no headers yet"))?;
    if tip.height != tip_height {
        bail!("headers end at {}, the tip is {tip_height}", tip.height);
    }
    let pace = pace(headers);
    let mut out = Vec::new();
    for d in deployments(net) {
        out.push(deployment_json(d, headers, &tip, pace, rpc, seeds, gate));
    }
    Ok(json!({
        "network": net.name(),
        "updated": now,
        "tip": {"height": tip.height, "time": tip.time, "mediantime": tip.mediantime},
        "dashd": dashd,
        "block_seconds": (pace * 10.0).round() / 10.0,
        "pace_blocks": (headers.len() as i64 - 1).min(PACE_BLOCKS),
        "mnhf": mnhf_quorum(net).name,
        "deployments": out,
        "error": null,
    }))
}

fn deployment_json(
    d: &Deployment,
    headers: &[Header],
    tip: &Header,
    pace: f64,
    rpc: &Value,
    seeds: &HashMap<String, (State, i64)>,
    gate: &HashMap<String, (i64, Value)>,
) -> Value {
    let r = &rpc[d.name]["bip9"];
    let known = !r.is_null();
    let ehf_height = r["ehf_height"].as_i64();
    let seed = seeds.get(d.name).copied().unwrap_or((State::Defined, 0));
    let windows = windows_of(d, headers);
    let judged = walk(d, &windows, seed, ehf_height);
    // The window the next block belongs to, and the one the tip is in.
    let current = judged.last().cloned();
    let tip_window = judged.iter().rev().find(|j| j.window.mined > 0).cloned();

    // Our reading against dashd's (rpc `status` is the tip's state, `status_next` the next block's).
    let mut mismatches: Vec<String> = Vec::new();
    if known {
        let mut check = |what: &str, ours: Value, theirs: &Value| {
            if !theirs.is_null() && ours != *theirs {
                mismatches.push(format!("{what}: ours {ours}, dashd {theirs}"));
            }
        };
        if let Some(c) = &current {
            check("status_next", json!(c.state.name()), &r["status_next"]);
        }
        if let Some(t) = &tip_window {
            check("status", json!(t.state.name()), &r["status"]);
            check("since", json!(if t.state == State::Defined { 0 } else { t.since }), &r["since"]);
        }
        // Statistics: elapsed and count cover the tip's window; threshold is the next block's (on a
        // window's last block that is already the next window's: versionbits.cpp GetStateStatisticsFor).
        let s = &r["statistics"];
        if !s.is_null() {
            if let Some(t) = &tip_window {
                check("statistics.elapsed", json!(t.window.mined), &s["elapsed"]);
                check("statistics.count", json!(t.window.count), &s["count"]);
            }
            // Core's attempt is 0 unless the next block's window is STARTED.
            if let Some(c) = &current {
                check("statistics.threshold", json!(c.threshold.unwrap_or(threshold(d, 0))), &s["threshold"]);
            }
        }
    }

    let window_json = |j: &Judged| {
        let end = j.window.start + d.window - 1;
        let mut v = json!({
            "start": at(j.window.start, headers, tip, pace),
            "end": at(end, headers, tip, pace),
            "state": j.state.name(),
            "since": j.since,
            "mined": j.window.mined,
            "count": j.window.count,
            "attempt": j.attempt,
            "threshold": j.threshold,
        });
        if let Some(t) = j.threshold {
            let remaining = d.window - j.window.mined;
            let needed = (t - j.window.count).max(0);
            v["needed"] = json!(needed);
            v["remaining"] = json!(remaining);
            v["possible"] = json!(needed <= remaining);
            v["reached"] = json!(j.window.count >= t);
        }
        v
    };

    let current_json = current.as_ref().map(|c| {
        let mut v = window_json(c);
        // Which of its blocks signalled so far, like getdeploymentinfo's `signalling` ('#' yes, '-' no).
        let first = headers.first().map_or(0, |h| h.height);
        let from = (c.window.start - first).max(0) as usize;
        let s: String = headers[from.min(headers.len())..]
            .iter()
            .map(|h| if signals(h.version, d.bit) { '#' } else { '-' })
            .collect();
        v["signalling"] = json!(s);
        v
    });

    // Windows worth listing: the signalling ones and what they led to (the first LOCKED_IN, ACTIVE or
    // FAILED one), plus the last three.
    let n = judged.len();
    let listed: Vec<Value> = judged
        .iter()
        .enumerate()
        .filter(|(i, j)| {
            let first_of_its_state = *i == 0 || judged[i - 1].state != j.state;
            j.state == State::Started || (j.state != State::Defined && first_of_its_state) || *i + 3 >= n
        })
        .map(|(_, j)| window_json(j))
        .collect();
    let listed = listed[listed.len().saturating_sub(MAX_WINDOWS)..].to_vec();

    let recent = &headers[headers.len().saturating_sub(RECENT_BLOCKS as usize)..];
    let recent_count = recent.iter().filter(|h| signals(h.version, d.bit)).count();

    let outlook = if !d.signed && ehf_height.is_none() && current.as_ref().is_some_and(|c| c.state == State::Defined) {
        // Nothing this release runs signs it, so no window can start yet.
        json!({"phase": "not_signed"})
    } else {
        outlook(d, headers, tip, pace, current.as_ref(), ehf_height)
    };
    let schedule: Vec<Value> = (0..)
        .map(|a| (a, threshold(d, a)))
        .take_while(|(a, t)| *a == 0 || *t > d.threshold_min || threshold(d, a - 1) > d.threshold_min)
        .map(|(a, t)| json!({"attempt": a, "threshold": t}))
        .collect();

    json!({
        "name": d.name,
        "params": {
            "bit": d.bit, "start_time": d.start, "timeout": (d.timeout != NO_TIMEOUT).then_some(d.timeout),
            "window": d.window, "threshold_start": d.threshold_start, "threshold_min": d.threshold_min,
            "falloff_coeff": d.falloff, "min_activation_height": d.min_activation_height, "ehf": true,
        },
        "signed": d.signed,
        "rpc": if known { rpc[d.name].clone() } else { Value::Null },
        "consistent": known.then_some(mismatches.is_empty()),
        "mismatches": mismatches,
        "state": current.as_ref().map(|c| c.state.name()),
        "since": current.as_ref().map(|c| c.since),
        "ehf": {
            "height": ehf_height,
            "block": ehf_height.map(|h| at(h, headers, tip, pace)),
            "gate": gate.get(d.name).map(|(_, v)| v.clone()),
        },
        "window": current_json,
        "windows": listed,
        "recent": {"blocks": recent.len(), "count": recent_count},
        "schedule": schedule,
        "outlook": outlook,
    })
}

/// What happens next and when (estimates beyond the tip).
fn outlook(
    d: &Deployment,
    headers: &[Header],
    tip: &Header,
    pace: f64,
    current: Option<&Judged>,
    ehf: Option<i64>,
) -> Value {
    let Some(c) = current else { return Value::Null };
    let w = d.window;
    let end = c.window.start + w - 1;
    let a = |h: i64| at(h, headers, tip, pace);
    match c.state {
        State::Defined => {
            // When the median time past reaches the start time: about when block H-5's time does.
            let reach = if tip.mediantime >= d.start {
                tip.height
            } else {
                tip.height + ((d.start - tip.time) as f64 / pace).ceil().max(0.0) as i64 + 5
            };
            // The signal can't be mined before then (its block's median time must be past the start).
            let signal = ehf.unwrap_or(reach + 1).max(reach);
            let first = signal / w * w + w;
            let started = (first..).step_by(w as usize).find(|b| *b > signal.max(reach)).unwrap_or(first);
            json!({
                "phase": if ehf.is_some() { "waiting_for_window" } else { "waiting_for_signal" },
                "start_time_reached": tip.mediantime >= d.start,
                "start_time_block": a(reach),
                "earliest_start": a(started),
                "margin_blocks": started - 1 - signal.max(reach),
                "earliest_lock_in": a(started + w),
                "earliest_activation": a(started + 2 * w),
                "first_threshold": threshold(d, 0),
            })
        }
        State::Started => {
            let t = c.threshold.unwrap_or(d.threshold_start);
            let remaining = w - c.window.mined;
            let needed = (t - c.window.count).max(0);
            json!({
                "phase": "signalling",
                "window_end": a(end),
                "needed": needed,
                "remaining": remaining,
                "possible": needed <= remaining,
                "lock_in_if_reached": a(end + 1),
                "activation_if_reached": a(end + 1 + w),
                "next_threshold": threshold(d, c.attempt.unwrap_or(0) + 1),
                "next_window": a(end + 1),
            })
        }
        State::LockedIn => json!({"phase": "locked_in", "activation": a(c.since + w)}),
        State::Active => json!({"phase": "active", "activation": a(c.since)}),
        State::Failed => json!({"phase": "failed", "failed": a(c.since)}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V24: Deployment = MAINNET[0];

    #[test]
    fn released_thresholds() {
        // 3226 - a²·4032/100/5, integer steps, floored at 2420 from the tenth failed window.
        let want = [3226, 3218, 3194, 3154, 3097, 3025, 2936, 2831, 2710, 2573, 2420, 2420, 2420];
        let got: Vec<i64> = (0..want.len() as i64).map(|a| threshold(&V24, a)).collect();
        assert_eq!(got, want);
        assert_eq!(threshold(&TESTNET[0], 0), 80);
        assert_eq!(threshold(&TESTNET[0], 10), 60);
    }

    #[test]
    fn condition_needs_the_top_bits() {
        assert!(signals(0x2000_2000, 13));
        assert!(!signals(0x2000_0000, 13));
        // Bit 13 set but not a versionbits block.
        assert!(!signals(0x6000_2000, 13));
        assert!(!signals(0x0000_2000, 13));
        assert!(signals(0x2000_6000, 14));
        // Negative i32 versions (top bit set) never signal.
        assert!(!signals(-0x7FFF_E000, 13));
    }

    #[test]
    fn request_id_matches_core() {
        // test/functional/feature_mnehf.py at v24.0.0 pins bit 28's.
        assert_eq!(request_id(28), "a0eee872d7d3170dd20d5c5e8380c92b3aa887da5f63d8033289fafa35a90691");
        assert_eq!(request_id(13), "c474f10ec3ff41beef91da44bce948772a816814c0f6d64d1bb23c9cc182fe1b");
    }

    fn full(start: i64, count: i64, mtp: i64) -> Window {
        Window { start, mined: V24.window, count, mtp_end: Some(mtp) }
    }

    const B: i64 = 2_572_416; // a window boundary (638 × 4032)

    #[test]
    fn defined_until_the_signal_and_the_clock() {
        let late = V24.start + 1;
        // Clock past, signal not mined: stays DEFINED.
        let j = walk(&V24, &[full(B - 4032, 0, late)], (State::Defined, 0), None);
        assert_eq!(j.last().unwrap().state, State::Defined);
        // Signal mined in the window's last block: the next window starts.
        let j = walk(&V24, &[full(B - 4032, 0, late)], (State::Defined, 0), Some(B - 1));
        assert_eq!((j[1].state, j[1].since, j[1].attempt, j[1].threshold), (State::Started, B, Some(0), Some(3226)));
        // Mined in the next window's first block: one window later.
        let j = walk(&V24, &[full(B - 4032, 0, late)], (State::Defined, 0), Some(B));
        assert_eq!(j[1].state, State::Defined);
        // Signal mined, clock not yet past the start at the boundary.
        let j = walk(&V24, &[full(B - 4032, 0, V24.start - 1)], (State::Defined, 0), Some(B - 10));
        assert_eq!(j[1].state, State::Defined);
    }

    #[test]
    fn lock_in_at_exactly_the_threshold() {
        let late = V24.start + 1;
        let j = walk(&V24, &[full(B, 3225, late)], (State::Started, B), Some(B - 5));
        assert_eq!((j[1].state, j[1].attempt, j[1].threshold), (State::Started, Some(1), Some(3218)));
        let j = walk(&V24, &[full(B, 3226, late)], (State::Started, B), Some(B - 5));
        assert_eq!((j[1].state, j[1].since), (State::LockedIn, B + 4032));
        // Then ACTIVE one window later.
        let j = walk(&V24, &[full(B, 3226, late), full(B + 4032, 0, late)], (State::Started, B), Some(B - 5));
        assert_eq!((j[2].state, j[2].since), (State::Active, B + 2 * 4032));
    }

    #[test]
    fn attempts_lower_the_threshold() {
        let late = V24.start + 1;
        let ws: Vec<Window> = (0..3).map(|i| full(B + i * 4032, 3200, late)).collect();
        let j = walk(&V24, &ws, (State::Started, B), Some(B - 5));
        // 3200 misses 3226 and 3218, then meets attempt 2's 3194.
        assert_eq!(j.iter().map(|x| x.threshold).collect::<Vec<_>>(), [Some(3226), Some(3218), Some(3194), None]);
        assert_eq!(j[3].state, State::LockedIn);
    }

    #[test]
    fn timeout_fails_a_started_deployment() {
        let j = walk(&V24, &[full(B, 10, V24.timeout)], (State::Started, B), Some(B - 5));
        assert_eq!(j[1].state, State::Failed);
        // Locking in wins over the timeout in the same window.
        let j = walk(&V24, &[full(B, 3300, V24.timeout)], (State::Started, B), Some(B - 5));
        assert_eq!(j[1].state, State::LockedIn);
    }

    #[test]
    fn partial_window_is_the_current_one() {
        let w = Window { start: B, mined: 100, count: 90, mtp_end: None };
        let j = walk(&V24, std::slice::from_ref(&w), (State::Started, B), Some(B - 5));
        assert_eq!(j.len(), 1);
        assert_eq!(j[0].window, w);
    }

    fn headers(from: i64, n: i64, version: impl Fn(i64) -> i64) -> Vec<Header> {
        (from..from + n)
            .map(|h| Header {
                height: h,
                time: 1_800_000_000 + h * 150,
                mediantime: 1_800_000_000 + h * 150 - 900,
                version: version(h),
            })
            .collect()
    }

    #[test]
    fn windows_from_headers_and_rollover() {
        let d = TESTNET[0];
        // Starts mid-window: that partial first window is dropped.
        let hs = headers(1050, 250, |h| if h % 2 == 0 { 0x2000_2000 } else { 0x2000_0000 });
        let ws = windows_of(&d, &hs);
        assert_eq!(
            ws.iter().map(|w| (w.start, w.mined, w.count)).collect::<Vec<_>>(),
            [(1100, 100, 50), (1200, 100, 50)]
        );
        assert!(ws.iter().all(|w| w.mtp_end.is_some()));
        // The tip on a window's last block: the next block opens a fresh window.
        let j = walk(&d, &ws, (State::Started, 1100), Some(1000));
        assert_eq!((j[2].window.start, j[2].window.mined, j[2].attempt), (1300, 0, Some(2)));
    }

    #[test]
    fn document_reads_like_dashd() {
        // Signal at 1040, start time long past: STARTED from 1100; 80 of 100 signal in 1200's window.
        let hs = headers(1000, 330, |h| if (1200..1280).contains(&h) { 0x2000_2000 } else { 0x2000_0000 });
        let rpc = json!({"v24": {"bip9": {"status": "locked_in", "status_next": "locked_in", "since": 1300, "ehf_height": 1040}}});
        let seeds = HashMap::from([("v24".to_owned(), (State::Defined, 0))]);
        let doc = build(Network::Testnet, &hs, 1329, &rpc, &seeds, &HashMap::new(), None, 0).unwrap();
        let v = &doc["deployments"][0];
        assert_eq!(v["state"], "locked_in");
        assert_eq!(v["consistent"], true, "{}", v["mismatches"]);
        assert_eq!(v["outlook"]["activation"]["height"], 1400);
        assert_eq!(v["window"]["signalling"].as_str().unwrap().len(), 30);
        // A disagreement is reported, not hidden.
        let rpc = json!({"v24": {"bip9": {"status": "started", "status_next": "started", "since": 1100, "ehf_height": 1040}}});
        let doc = build(Network::Testnet, &hs, 1329, &rpc, &seeds, &HashMap::new(), None, 0).unwrap();
        assert_eq!(doc["deployments"][0]["consistent"], false);
    }

    #[test]
    fn waiting_for_the_start_estimates_the_first_window() {
        // Mainnet as of 2026-11-06: tip 2,567,594, about 2.8 days before the start time.
        let tip = 2_567_594;
        let t0 = V24.start - 240_000;
        // From a window boundary, as the collector stores them.
        let hs: Vec<Header> = (2_564_352..=tip)
            .map(|h| Header {
                height: h,
                time: t0 - (tip - h) * 158,
                mediantime: t0 - (tip - h) * 158 - 900,
                version: 0x2000_0000,
            })
            .collect();
        let seeds = HashMap::from([("v24".to_owned(), (State::Defined, 0))]);
        let doc = build(Network::Mainnet, &hs, tip, &json!({}), &seeds, &HashMap::new(), None, 0).unwrap();
        let o = &doc["deployments"][0]["outlook"];
        assert_eq!(o["phase"], "waiting_for_signal");
        assert_eq!(o["earliest_start"]["height"], B);
        assert_eq!(o["earliest_lock_in"]["height"], B + 4032);
        assert_eq!(o["earliest_activation"]["height"], 2_580_480);
        assert_eq!(o["earliest_start"]["estimated"], true);
        // No dashd reading: nothing to compare.
        assert_eq!(doc["deployments"][0]["consistent"], Value::Null);
    }
}

#[cfg(test)]
mod fixture {
    use super::*;

    /// Writes a mainnet document mid-signalling (attempt 2, after two short windows) to $ACTIVATION_FIXTURE,
    /// for checking the page by hand: `ACTIVATION_FIXTURE=/tmp/x.json cargo test fixture -- --ignored`.
    #[test]
    #[ignore]
    fn started_mainnet() {
        let Ok(path) = std::env::var("ACTIVATION_FIXTURE") else { return };
        let d = MAINNET[0];
        let first = 2_572_416 - 4032;
        let tip = 2_572_416 + 2 * 4032 + 2900;
        let t0 = d.start + 3 * 86400;
        let hs: Vec<Header> = (first..=tip)
            .map(|h| {
                let i = h - 2_572_416;
                let signal = match i {
                    i if i < 0 => false,
                    i if i < 4032 => i % 10 < 7,   // 70%: short of 80%
                    i if i < 8064 => i % 100 < 79, // 79%: short of 79.8%
                    i => (i * 7919) % 100 < 81,    // 81% so far this window
                };
                let time = t0 + (h - 2_572_416) * 157;
                Header {
                    height: h,
                    time,
                    mediantime: time - 900,
                    version: if signal { 0x2000_2000 } else { 0x2000_0000 },
                }
            })
            .collect();
        let rpc = json!({"v24": {"type": "bip9", "bip9": {"status": "started", "status_next": "started", "since": 2_572_416, "ehf_height": 2_569_300, "ehf": true}}});
        let seeds =
            HashMap::from([("v24".to_owned(), (State::Defined, 0)), ("evo_shares".to_owned(), (State::Defined, 0))]);
        let doc =
            build(Network::Mainnet, &hs, tip, &rpc, &seeds, &HashMap::new(), Some("/Dash Core:24.0.0/".into()), t0)
                .unwrap();
        std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    }
}

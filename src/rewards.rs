//! Platform rewards for evonodes. Each epoch's pool (fees, plus the Core block rewards paid into
//! Platform) goes, once the next epoch starts, to the evonodes that proposed its blocks, pro rata by
//! blocks, into each one's masternode identity (its id is the ProTx hash's bytes as displayed). Drive
//! records every finalized epoch with its proposers (from epoch 34 on mainnet), so what each evonode
//! earned is worked out here exactly as Drive paid it (`payouts`).
//!
//! Every POLL_INTERVAL: new finalized epochs, the current epoch and its blocks so far, identity
//! balances (what each evonode can withdraw), the protocol upgrade vote, and some lifetime totals from
//! Platform Explorer (third party) for the epochs before 34. Every QUORUM_INTERVAL: the validator sets,
//! for when each evonode proposes next and which turns it misses.
//!
//! The calls are unproved gRPC-web, as in platform.rs, to evonodes that passed its checks. An answer
//! counts only once two evonodes give the same one; finalized epochs never change, so each is checked
//! once, when it's stored.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rand::seq::SliceRandom;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::db::{self, get_meta, query_all, set_meta};
use crate::network::Network;
use crate::platform::{self, Field, fields, msg, uint};
use crate::state::NetCtx;

const POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// A mainnet block takes about 140 s: sampling more often sees nearly every proposer turn.
const QUORUM_INTERVAL: Duration = Duration::from_secs(90);
/// Most items Drive returns per query (`max_returned_elements`, and the ID limit of getIdentitiesBalances).
const PAGE: usize = 100;
/// Largest answer read: 100 finalized epochs of ~360 proposers is about 1.5 MB.
const MAX_ANSWER: usize = 16 << 20;
/// Evonodes asked before giving up on two matching answers.
const MAX_ASKED: usize = 6;
/// Epoch lists published per masternode and in the network document (a testnet epoch is an hour).
pub const EPOCHS_SHOWN: i64 = 120;
const CREDITS_PER_DASH: f64 = 1e11;
const EVO_COLLATERAL_DASH: f64 = 4_000.0;
const DAY: i64 = 86_400;

// ---- requests ------------------------------------------------------------------------------------

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_uint(out: &mut Vec<u8>, num: u64, v: u64) {
    put_varint(out, num << 3);
    put_varint(out, v);
}

fn put_bytes(out: &mut Vec<u8>, num: u64, b: &[u8]) {
    put_varint(out, (num << 3) | 2);
    put_varint(out, b.len() as u64);
    out.extend(b);
}

/// Every request is `{ v0 = 1: { … } }`.
fn v0(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    put_bytes(&mut out, 1, body);
    out
}

// ---- answers -------------------------------------------------------------------------------------

/// Every length-delimited field `num` of a message (a repeated message or bytes field).
fn all(buf: &[u8], num: u64) -> Result<Vec<&[u8]>> {
    Ok(fields(buf)?
        .into_iter()
        .filter_map(|(n, f)| match f {
            Field::Bytes(b) if n == num => Some(b),
            _ => None,
        })
        .collect())
}

fn id32(b: &[u8]) -> Result<[u8; 32]> {
    b.try_into().map_err(|_| anyhow!("identifier of {} bytes", b.len()))
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// ResponseMetadata: where the answering node's chain was.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Meta {
    pub height: u64,
    pub core_height: u64,
    pub epoch: u64,
    pub time_ms: u64,
    pub protocol_version: u64,
}

fn meta(v0: &[u8], num: u64) -> Meta {
    let m = msg(v0, num).unwrap_or_default();
    Meta {
        height: uint(m, 1).unwrap_or(0),
        core_height: uint(m, 2).unwrap_or(0),
        epoch: uint(m, 3).unwrap_or(0),
        time_ms: uint(m, 4).unwrap_or(0),
        protocol_version: uint(m, 5).unwrap_or(0),
    }
}

/// The `v0` of an answer and its result (field 1); a proof (field 2) means we asked wrong.
fn result(resp: &[u8]) -> Result<(&[u8], &[u8], Meta)> {
    let v0 = msg(resp, 1).ok_or_else(|| anyhow!("no v0 answer"))?;
    let result = msg(v0, 1).unwrap_or_default();
    Ok((v0, result, meta(v0, 3)))
}

/// One finalized epoch (FinalizedEpochInfo).
#[derive(Clone, Debug, PartialEq)]
pub struct Finalized {
    pub epoch: u64,
    pub first_block_height: u64,
    pub first_core_height: u64,
    pub first_block_time_ms: u64,
    pub protocol_version: u64,
    pub total_blocks: u64,
    pub next_core_height: u64,
    pub processing_fees: u64,
    pub storage_fees: u64,
    pub core_rewards: u64,
    /// (identity id = ProTx hash as displayed, blocks proposed).
    pub proposers: Vec<([u8; 32], u64)>,
}

impl Finalized {
    /// What Drive paid out for the epoch: processing fees, the epoch's share of storage fees, and
    /// the Core block rewards for its Core blocks.
    pub fn pool(&self) -> u64 {
        self.processing_fees + self.storage_fees + self.core_rewards
    }
}

/// GetFinalizedEpochInfosResponse: v0 { epochs = 1 { finalized_epoch_infos = 1 [ { number = 1,
/// first_block_height = 2, first_core_block_height = 3, first_block_time = 4, protocol_version = 6,
/// total_blocks_in_epoch = 7, next_epoch_start_core_block_height = 8, total_processing_fees = 9,
/// total_distributed_storage_fees = 10, core_block_rewards = 12, block_proposers = 13 [ { proposer_id
/// = 1, block_count = 2 } ] } ] }, metadata = 3 }.
pub fn parse_finalized(resp: &[u8]) -> Result<(Vec<Finalized>, Meta)> {
    let (_, epochs, meta) = result(resp)?;
    let mut out = Vec::new();
    for e in all(epochs, 1)? {
        let u = |n| uint(e, n).unwrap_or(0);
        let mut proposers = Vec::new();
        for p in all(e, 13)? {
            proposers.push((id32(msg(p, 1).unwrap_or_default())?, uint(p, 2).unwrap_or(0)));
        }
        out.push(Finalized {
            epoch: u(1),
            first_block_height: u(2),
            first_core_height: u(3),
            first_block_time_ms: u(4),
            protocol_version: u(6),
            total_blocks: u(7),
            next_core_height: u(8),
            processing_fees: u(9),
            storage_fees: u(10),
            core_rewards: u(12),
            proposers,
        });
    }
    Ok((out, meta))
}

/// The current epoch (EpochInfo).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Epoch {
    pub epoch: u64,
    pub first_block_height: u64,
    pub first_core_height: u64,
    pub start_ms: u64,
    pub protocol_version: u64,
}

/// GetEpochsInfoResponse: v0 { epochs = 1 { epoch_infos = 1 [ { number = 1, first_block_height = 2,
/// first_core_block_height = 3, start_time = 4, protocol_version = 6 } ] }, metadata = 3 }.
pub fn parse_epochs(resp: &[u8]) -> Result<(Vec<Epoch>, Meta)> {
    let (_, epochs, meta) = result(resp)?;
    let list = all(epochs, 1)?
        .into_iter()
        .map(|e| Epoch {
            epoch: uint(e, 1).unwrap_or(0),
            first_block_height: uint(e, 2).unwrap_or(0),
            first_core_height: uint(e, 3).unwrap_or(0),
            start_ms: uint(e, 4).unwrap_or(0),
            protocol_version: uint(e, 6).unwrap_or(0),
        })
        .collect();
    Ok((list, meta))
}

pub type Pairs = Vec<([u8; 32], Option<u64>)>;

/// A list of (32-byte id, number) pairs, the shape of several answers: GetEvonodesProposedEpochBlocksResponse
/// (v0 { evonodes_proposed_block_counts_info = 1 { evonodes_proposed_block_counts = 1 [ { pro_tx_hash = 1,
/// count = 2 } ] } }), GetIdentitiesBalancesResponse (v0 { identities_balances = 1 { entries = 1 [ {
/// identity_id = 1, balance = 2 } ] } }; an identity without a balance has no field 2) and
/// GetProtocolVersionUpgradeVoteStatusResponse (v0 { versions = 1 { version_signals = 1 [ {
/// pro_tx_hash = 1, version = 2 } ] } }).
pub fn parse_pairs(resp: &[u8]) -> Result<(Pairs, Meta)> {
    let (_, list, meta) = result(resp)?;
    let mut out = Vec::new();
    for e in all(list, 1)? {
        out.push((id32(msg(e, 1).unwrap_or_default())?, uint(e, 2)));
    }
    Ok((out, meta))
}

/// GetProtocolVersionUpgradeStateResponse: v0 { versions = 1 { versions = 1 [ { version_number = 1,
/// vote_count = 2 } ] }, metadata = 3 }. (version, votes), in the answer's order.
pub fn parse_upgrade_state(resp: &[u8]) -> Result<(Vec<(u64, u64)>, Meta)> {
    let (_, list, meta) = result(resp)?;
    let out = all(list, 1)?.into_iter().map(|e| (uint(e, 1).unwrap_or(0), uint(e, 2).unwrap_or(0))).collect();
    Ok((out, meta))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    /// As displayed, like every other id here: Drive's validator sets are keyed and ordered by these bytes.
    pub pro_tx_hash: [u8; 32],
    pub banned: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValidatorSet {
    pub quorum_hash: Vec<u8>,
    /// Sorted by `pro_tx_hash`: Tenderdash's proposer order.
    pub members: Vec<Member>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Quorums {
    /// Every validator set's quorum hash, in Drive's rotation order.
    pub order: Vec<Vec<u8>>,
    pub current: Vec<u8>,
    pub sets: Vec<ValidatorSet>,
    pub last_proposer: [u8; 32],
}

/// GetCurrentQuorumsInfoResponse: v0 { quorum_hashes = 1 [], current_quorum_hash = 2, validator_sets = 3
/// [ { quorum_hash = 1, members = 3 [ { pro_tx_hash = 1, is_banned = 3 } ] } ], last_block_proposer = 4,
/// metadata = 5 }.
pub fn parse_quorums(resp: &[u8]) -> Result<(Quorums, Meta)> {
    let v0 = msg(resp, 1).ok_or_else(|| anyhow!("no v0 answer"))?;
    let mut sets = Vec::new();
    for s in all(v0, 3)? {
        let mut members = Vec::new();
        for m in all(s, 3)? {
            members.push(Member { pro_tx_hash: id32(msg(m, 1).unwrap_or_default())?, banned: uint(m, 3) == Some(1) });
        }
        members.sort_by_key(|m| m.pro_tx_hash);
        sets.push(ValidatorSet { quorum_hash: msg(s, 1).unwrap_or_default().to_vec(), members });
    }
    let q = Quorums {
        order: all(v0, 1)?.into_iter().map(<[u8]>::to_vec).collect(),
        current: msg(v0, 2).unwrap_or_default().to_vec(),
        sets,
        last_proposer: id32(msg(v0, 4).unwrap_or_default()).context("last block proposer")?,
    };
    Ok((q, meta(v0, 5)))
}

// ---- Drive's rules -------------------------------------------------------------------------------

/// What each proposer was paid for an epoch, exactly as Drive pays it
/// (add_epoch_pool_to_proposers_payout_operations): in identifier order, `pool × blocks / total`
/// rounded down, and the last proposer also gets what the divisions left over. Reward shares (none on
/// mainnet) aren't deducted, so this is the evonode's gross.
pub fn payouts(pool: u64, total_blocks: u64, proposers: &[([u8; 32], u64)]) -> Vec<([u8; 32], u64, u64)> {
    let mut sorted = proposers.to_vec();
    sorted.sort_by_key(|p| p.0);
    let mut remaining = pool;
    let n = sorted.len();
    sorted
        .into_iter()
        .enumerate()
        .map(|(i, (id, blocks))| {
            let share = if total_blocks == 0 {
                0
            } else {
                (u128::from(pool) * u128::from(blocks) / u128::from(total_blocks)) as u64
            };
            remaining = remaining.saturating_sub(share);
            let paid = if i + 1 == n { share + remaining } else { share };
            (id, blocks, paid)
        })
        .collect()
}

/// Core block rewards Platform gets for Core blocks `from..=to` (rs-dpp
/// epoch_core_reward_credits_for_distribution v0): 37.5% of the masternodes' 60% of the block subsidy,
/// starting at 5 DASH and cut by 1/14 every `halving` blocks.
pub fn core_rewards(from: u64, to: u64, halving: u64) -> u64 {
    const GENESIS: u64 = 112_500_000_000;
    if from > to || halving == 0 {
        return 0;
    }
    let per_block = |year: u64| (0..year).fold(GENESIS, |d, _| d - d / 14);
    let (prev_from, prev_to) = (from.saturating_sub(1), to.saturating_sub(1));
    let (first_year, last_year) = (prev_from / halving, prev_to / halving);
    (first_year..=last_year)
        .map(|y| {
            let start = if y == first_year { prev_from } else { y * halving };
            let end = if y == last_year { to } else { (y + 1) * halving };
            (end - start) * per_block(y)
        })
        .sum()
}

/// Core's subsidy halving interval (the same on mainnet and testnet).
const CORE_HALVING: u64 = 210_240;

/// Votes an upgrade needs at the epoch change: `1 + floor(active × 67 / 100)` (check_for_desired_protocol_upgrade).
pub fn votes_required(active_evonodes: u64) -> u64 {
    1 + active_evonodes * 67 / 100
}

/// The turn order from a validator-set sample, mirroring Drive's validator_set_update_v2 and Tenderdash's
/// proposer selection: the rest of the active set after the last proposer, then each other set in
/// rotation order (wrapping, never into the two oldest when there are more than ten), each in ProTx
/// order. Banned members are skipped. Returns each evonode's first turn: (ProTx hash, blocks from now,
/// 1 = the next block).
pub fn schedule(q: &Quorums) -> Vec<([u8; 32], u64)> {
    let sets: HashMap<&[u8], &ValidatorSet> = q.sets.iter().map(|s| (s.quorum_hash.as_slice(), s)).collect();
    let order: Vec<&[u8]> = q.order.iter().map(Vec::as_slice).filter(|h| sets.contains_key(h)).collect();
    let mut out = Vec::new();
    if order.is_empty() {
        return out;
    }
    let mut seen = HashSet::new();
    let mut slot = 0;
    let mut turn = |m: &Member, out: &mut Vec<([u8; 32], u64)>| {
        slot += 1;
        if seen.insert(m.pro_tx_hash) {
            out.push((m.pro_tx_hash, slot));
        }
    };
    let current = order.iter().position(|h| *h == q.current.as_slice()).unwrap_or(0);
    let rotatable = if order.len() > 10 { order.len() - 2 } else { order.len() };
    for m in sets[order[current]].members.iter().filter(|m| !m.banned && m.pro_tx_hash > q.last_proposer) {
        turn(m, &mut out);
    }
    let mut index = current;
    for _ in 0..rotatable.saturating_sub(1) {
        index = if index + 1 >= rotatable { 0 } else { index + 1 };
        if index == current {
            continue;
        }
        for m in sets[order[index]].members.iter().filter(|m| !m.banned) {
            turn(m, &mut out);
        }
    }
    out
}

/// Turns between two samples of the same validator set, when they're certain: from just after `from`
/// up to `to`, every unbanned member had one, and `blocks` blocks were made. If that's as many blocks
/// as turns, everyone proposed; if it's one, only `to` did and the rest missed. Anything else can't be
/// pinned on anyone. Returns (member, missed).
pub fn turns_between(set: &ValidatorSet, from: &[u8; 32], to: &[u8; 32], blocks: u64) -> Option<Vec<([u8; 32], bool)>> {
    if to <= from || blocks == 0 {
        return None;
    }
    let passed: Vec<[u8; 32]> = set
        .members
        .iter()
        .filter(|m| !m.banned && m.pro_tx_hash > *from && m.pro_tx_hash <= *to)
        .map(|m| m.pro_tx_hash)
        .collect();
    if passed.last() != Some(to) {
        return None;
    }
    let n = passed.len() as u64;
    if blocks == n {
        Some(passed.into_iter().map(|p| (p, false)).collect())
    } else if blocks == 1 {
        Some(passed.into_iter().map(|p| (p, p != *to)).collect())
    } else {
        None
    }
}

/// How many rotating validator sets each unbanned member is in (sets past the two oldest of more than
/// ten never get a turn). Over time, its expected share of blocks is its count over everyone's.
fn memberships(q: &Quorums) -> HashMap<[u8; 32], u64> {
    let rotatable = if q.order.len() > 10 { q.order.len() - 2 } else { q.order.len() };
    let mut out = HashMap::new();
    for hash in q.order.iter().take(rotatable) {
        if let Some(set) = q.sets.iter().find(|s| &s.quorum_hash == hash) {
            for m in set.members.iter().filter(|m| !m.banned) {
                *out.entry(m.pro_tx_hash).or_default() += 1;
            }
        }
    }
    out
}

// ---- asking evonodes -----------------------------------------------------------------------------

/// Evonodes to ask, in random order: those that passed platform.rs's latest check, or every enabled
/// evonode with an address if too few have.
struct Peers {
    list: Vec<SocketAddr>,
    next: usize,
    client: reqwest::Client,
}

impl Peers {
    async fn load(ctx: &NetCtx, client: &reqwest::Client) -> Result<Peers> {
        let since = db::now() - 3600;
        let (healthy, all): (Vec<String>, Vec<String>) = ctx
            .db
            .read(move |c| {
                const ENABLED: &str = "m.type = 'Evo' AND m.removed IS NULL AND coalesce(m.pose_ban_height, -1) <= 0
                    AND m.platform_https IS NOT NULL";
                let healthy = query_all(
                    c,
                    &format!(
                        "SELECT m.platform_https FROM masternodes m JOIN platform_status s ON s.protx = m.protx
                         WHERE {ENABLED} AND s.ts > ?1 AND s.https_error IS NULL AND coalesce(s.catching_up, 0) = 0
                           AND (s.node_protx IS NULL OR s.node_protx = m.protx)"
                    ),
                    [since],
                    |r| r.get(0),
                )?;
                let all =
                    query_all(c, &format!("SELECT m.platform_https FROM masternodes m WHERE {ENABLED}"), [], |r| {
                        r.get(0)
                    })?;
                Ok((healthy, all))
            })
            .await?;
        let pick = if healthy.len() >= 4 { healthy } else { all };
        let mut list: Vec<SocketAddr> = pick.iter().filter_map(|a| platform::target(a).ok()).collect();
        list.shuffle(&mut rand::rng());
        if list.len() < 2 {
            bail!("fewer than two evonodes to ask");
        }
        Ok(Peers { list, next: 0, client: client.clone() })
    }

    fn take(&mut self) -> SocketAddr {
        let a = self.list[self.next % self.list.len()];
        self.next += 1;
        a
    }

    /// Asks evonodes in turn until two in a row give the same answer (metadata aside), and returns it
    /// with the newer metadata.
    async fn agreed<T: PartialEq>(
        &mut self,
        method: &str,
        request: &[u8],
        parse: impl Fn(&[u8]) -> Result<(T, Meta)>,
    ) -> Result<(T, Meta)> {
        let mut last: Option<(T, Meta, SocketAddr)> = None;
        let mut error = None;
        for _ in 0..MAX_ASKED.min(self.list.len() + 1) {
            let addr = self.take();
            let answer = match platform::unary(&self.client, addr, method, request, MAX_ANSWER).await {
                Ok((m, _)) => parse(&m),
                Err(e) => Err(e),
            };
            match answer {
                Ok((t, m)) => {
                    if let Some((prev, pm, _)) = &last
                        && *prev == t
                    {
                        return Ok((t, if pm.height > m.height { *pm } else { m }));
                    }
                    last = Some((t, m, addr));
                }
                Err(e) => error = Some(format!("{addr}: {e:#}")),
            }
        }
        match (last, error) {
            (Some((_, _, addr)), _) => bail!("{method}: no second evonode agreed with {addr}"),
            (None, e) => bail!("{method}: {}", e.unwrap_or_else(|| "no answer".into())),
        }
    }
}

// ---- the collector -------------------------------------------------------------------------------

/// The latest validator-set sample: who proposes when.
#[derive(Clone, Debug, Default)]
pub struct Schedule {
    pub ts: i64,
    pub height: u64,
    pub epoch: u64,
    /// Seconds per block the times below assume.
    pub block_secs: f64,
    /// Blocks until each evonode's next turn (ProTx as displayed → 1 for the next block).
    pub slots: HashMap<String, u64>,
}

impl Schedule {
    /// When `protx` is expected to propose next (unix time), if it's in a rotating validator set.
    pub fn eta(&self, protx: &str) -> Option<i64> {
        self.slots.get(protx).map(|&s| self.ts + (s as f64 * self.block_secs).round() as i64)
    }
}

/// The current epoch as last read, kept in `meta` under NOW_KEY.
const NOW_KEY: &str = "platform_now";

async fn wait_for_evonodes(ctx: &NetCtx) {
    loop {
        let n: i64 = ctx
            .db
            .read(|c| {
                Ok(c.query_row("SELECT count(*) FROM masternodes WHERE type = 'Evo' AND removed IS NULL", [], |r| {
                    r.get(0)
                })?)
            })
            .await
            .unwrap_or(0);
        if n > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

pub async fn run(ctx: Arc<NetCtx>) {
    let client = platform::client();
    let explorer = explorer_client();
    wait_for_evonodes(&ctx).await;
    loop {
        if let Err(e) = round(&ctx, &client, &explorer).await {
            tracing::warn!(net = ctx.net.name(), "platform rewards: {e:#}");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn round(ctx: &NetCtx, client: &reqwest::Client, explorer: &reqwest::Client) -> Result<()> {
    let mut peers = Peers::load(ctx, client).await?;
    // The current epoch: getEpochsInfo { count = 2: 1, ascending = 3: false }, from the latest epoch.
    let (epochs, meta) = peers.agreed("getEpochsInfo", &v0(&[0x10, 0x01]), parse_epochs).await?;
    let current = *epochs.first().ok_or_else(|| anyhow!("getEpochsInfo: no epoch"))?;
    let stored = finalized(ctx, &mut peers, current.epoch).await?;
    let blocks = epoch_blocks(&mut peers, current.epoch).await?;
    let ids: Vec<String> = ctx
        .db
        .read(|c| {
            Ok(query_all(c, "SELECT protx FROM masternodes WHERE type = 'Evo' AND removed IS NULL", [], |r| r.get(0))?)
        })
        .await?;
    let balances = balances(&mut peers, &ids).await?;
    let (versions, _) = peers.agreed("getProtocolVersionUpgradeState", &v0(&[]), parse_upgrade_state).await?;
    let votes = votes(&mut peers).await?;
    let now = json!({
        "epoch": current.epoch, "start": current.start_ms / 1000, "first_height": current.first_block_height,
        "first_core_height": current.first_core_height, "protocol_version": meta.protocol_version.max(current.protocol_version),
        "height": meta.height, "core_height": meta.core_height, "time": meta.time_ms / 1000, "versions": versions,
        "updated": db::now(),
    });
    let n = (blocks.len(), balances.len(), votes.len());
    ctx.db
        .write(move |c| {
            let tx = c.transaction()?;
            let now_ts = db::now();
            tx.execute("DELETE FROM platform_epoch_blocks", [])?;
            {
                let mut st =
                    tx.prepare("INSERT INTO platform_epoch_blocks (protx, epoch, blocks) VALUES (?1, ?2, ?3)")?;
                for (id, b) in &blocks {
                    st.execute(params![hex(id), current.epoch as i64, *b as i64])?;
                }
                let mut latest = tx.prepare(
                    "INSERT INTO platform_balances (protx, credits, ts) VALUES (?1, ?2, ?3)
                     ON CONFLICT(protx) DO UPDATE SET credits = excluded.credits, ts = excluded.ts",
                )?;
                let mut history = tx.prepare(
                    "INSERT OR IGNORE INTO platform_balance_history (protx, ts, credits)
                     SELECT ?1, ?2, ?3 WHERE ?3 IS NOT (SELECT credits FROM platform_balance_history
                        WHERE protx = ?1 ORDER BY ts DESC LIMIT 1)",
                )?;
                for (id, credits) in &balances {
                    let credits = credits.map(|v| v as i64);
                    latest.execute(params![hex(id), credits, now_ts])?;
                    if let Some(v) = credits {
                        history.execute(params![hex(id), now_ts, v])?;
                    }
                }
                tx.execute("DELETE FROM platform_votes", [])?;
                let mut st =
                    tx.prepare("INSERT OR REPLACE INTO platform_votes (protx, version, ts) VALUES (?1, ?2, ?3)")?;
                for (id, v) in &votes {
                    st.execute(params![hex(id), *v as i64, now_ts])?;
                }
            }
            set_meta(&tx, NOW_KEY, &now.to_string())?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    tracing::info!(
        net = ctx.net.name(),
        "platform rewards: epoch {}, {stored} new finalized, {} proposers so far, {} balances, {} votes",
        current.epoch,
        n.0,
        n.1,
        n.2
    );
    if ctx.net == Network::Mainnet {
        explorer_totals(ctx, explorer).await;
    }
    Ok(())
}

/// Fetches and stores finalized epochs after the newest stored one, up to the one before `current`.
/// Returns how many were stored.
async fn finalized(ctx: &NetCtx, peers: &mut Peers, current: u64) -> Result<usize> {
    let last: Option<i64> =
        ctx.db.read(|c| Ok(c.query_row("SELECT max(epoch) FROM platform_epochs", [], |r| r.get(0))?)).await?;
    let mut start = last.map_or(0, |l| l as u64 + 1);
    let mut stored = 0;
    while start < current {
        let end = (start + PAGE as u64 - 1).min(current - 1);
        // getFinalizedEpochInfos { start_epoch_index = 1, start_included = 2, end_epoch_index = 3, end_included = 4 }.
        let mut req = Vec::new();
        put_uint(&mut req, 1, start);
        put_uint(&mut req, 2, 1);
        put_uint(&mut req, 3, end);
        put_uint(&mut req, 4, 1);
        let (epochs, _) = peers.agreed("getFinalizedEpochInfos", &v0(&req), parse_finalized).await?;
        stored += epochs.len();
        if !epochs.is_empty() {
            ctx.db.write(move |c| store_finalized(c, &epochs)).await?;
        }
        start = end + 1;
    }
    Ok(stored)
}

pub fn store_finalized(c: &mut Connection, epochs: &[Finalized]) -> Result<()> {
    let tx = c.transaction()?;
    {
        let mut ep = tx.prepare(
            "INSERT OR REPLACE INTO platform_epochs (epoch, first_block_height, first_block_time, first_core_height,
                next_core_height, total_blocks, processing_fees, storage_fees, core_rewards, protocol_version, proposers)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        )?;
        let mut pay = tx.prepare(
            "INSERT OR REPLACE INTO platform_payouts (epoch, protx, blocks, credits) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for e in epochs {
            ep.execute(params![
                e.epoch as i64,
                e.first_block_height as i64,
                (e.first_block_time_ms / 1000) as i64,
                e.first_core_height as i64,
                e.next_core_height as i64,
                e.total_blocks as i64,
                e.processing_fees as i64,
                e.storage_fees as i64,
                e.core_rewards as i64,
                e.protocol_version as i64,
                e.proposers.len() as i64,
            ])?;
            for (id, blocks, credits) in payouts(e.pool(), e.total_blocks, &e.proposers) {
                pay.execute(params![e.epoch as i64, hex(&id), blocks as i64, credits as i64])?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// Blocks each evonode proposed so far in `epoch`: getEvonodesProposedEpochBlocksByRange { epoch = 1,
/// limit = 2, start_after = 3 }, a page at a time.
async fn epoch_blocks(peers: &mut Peers, epoch: u64) -> Result<Vec<([u8; 32], u64)>> {
    let mut out: Vec<([u8; 32], u64)> = Vec::new();
    loop {
        let mut req = Vec::new();
        put_uint(&mut req, 1, epoch);
        put_uint(&mut req, 2, PAGE as u64);
        if let Some((last, _)) = out.last() {
            put_bytes(&mut req, 3, last);
        }
        let (page, _) = peers.agreed("getEvonodesProposedEpochBlocksByRange", &v0(&req), parse_pairs).await?;
        let n = page.len();
        out.extend(page.into_iter().map(|(id, b)| (id, b.unwrap_or(0))));
        if n < PAGE {
            return Ok(out);
        }
    }
}

/// Identity balances (credits) of the evonodes' masternode identities, 100 per getIdentitiesBalances { ids = 1 }.
async fn balances(peers: &mut Peers, protx: &[String]) -> Result<Vec<([u8; 32], Option<u64>)>> {
    let ids: Vec<[u8; 32]> = protx.iter().filter_map(|p| unhex(p)).collect();
    let mut out = Vec::new();
    for chunk in ids.chunks(PAGE) {
        let mut req = Vec::new();
        for id in chunk {
            put_bytes(&mut req, 1, id);
        }
        out.extend(peers.agreed("getIdentitiesBalances", &v0(&req), parse_pairs).await?.0);
    }
    Ok(out)
}

/// Every evonode's protocol version vote: getProtocolVersionUpgradeVoteStatus { start_pro_tx_hash = 1,
/// count = 2 }, a page at a time from the first ProTx (each page starts at, and repeats, the last one).
async fn votes(peers: &mut Peers) -> Result<BTreeMap<[u8; 32], u64>> {
    let mut out = BTreeMap::new();
    let mut start: Option<[u8; 32]> = None;
    loop {
        let mut req = Vec::new();
        if let Some(s) = &start {
            put_bytes(&mut req, 1, s);
        }
        put_uint(&mut req, 2, PAGE as u64);
        let (page, _) = peers.agreed("getProtocolVersionUpgradeVoteStatus", &v0(&req), parse_pairs).await?;
        let before = out.len();
        for (id, v) in &page {
            out.insert(*id, v.unwrap_or(0));
        }
        let last = page.last().map(|p| p.0);
        if page.len() < PAGE || out.len() == before || last == start {
            return Ok(out);
        }
        start = last;
    }
}

// ---- validator sets ------------------------------------------------------------------------------

pub async fn run_quorums(ctx: Arc<NetCtx>) {
    let client = platform::client();
    wait_for_evonodes(&ctx).await;
    let mut prev: Option<(Quorums, Meta)> = None;
    loop {
        match sample(&ctx, &client, prev.as_ref()).await {
            Ok(s) => prev = Some(s),
            Err(e) => tracing::debug!(net = ctx.net.name(), "validator sets: {e:#}"),
        }
        tokio::time::sleep(QUORUM_INTERVAL).await;
    }
}

async fn sample(ctx: &NetCtx, client: &reqwest::Client, prev: Option<&(Quorums, Meta)>) -> Result<(Quorums, Meta)> {
    let mut peers = Peers::load(ctx, client).await?;
    let (q, meta) = peers.agreed("getCurrentQuorumsInfo", &v0(&[]), parse_quorums).await?;
    let net = ctx.net;
    let block_secs = ctx.db.read(move |c| block_secs(c, net, meta.height, meta.time_ms / 1000)).await?;
    let slots = schedule(&q).into_iter().map(|(raw, slot)| (hex(&raw), slot)).collect();
    *ctx.platform_schedule.write().unwrap() = Some(Arc::new(Schedule {
        ts: (meta.time_ms / 1000) as i64,
        height: meta.height,
        epoch: meta.epoch,
        block_secs,
        slots,
    }));
    // Turns since the previous sample, when it was of the same set and epoch.
    let mut turns = Vec::new();
    if let Some((pq, pm)) = prev
        && pq.current == q.current
        && pm.epoch == meta.epoch
        && meta.height > pm.height
        && let Some(set) = q.sets.iter().find(|s| s.quorum_hash == q.current)
    {
        turns = turns_between(set, &pq.last_proposer, &q.last_proposer, meta.height - pm.height).unwrap_or_default();
    }
    if let Some(set) = q.sets.iter().find(|s| s.quorum_hash == q.current) {
        tracing::debug!(
            net = ctx.net.name(),
            "validator sets: height {}, set {} of {}, proposer {} of {}, {} turns pinned",
            meta.height,
            q.order.iter().position(|h| *h == q.current).map_or(0, |i| i + 1),
            q.order.len(),
            set.members.iter().position(|m| m.pro_tx_hash == q.last_proposer).map_or(0, |i| i + 1),
            set.members.len(),
            turns.len()
        );
    }
    let members = memberships(&q);
    let epoch = meta.epoch as i64;
    ctx.db
        .write(move |c| {
            let tx = c.transaction()?;
            {
                let mut st = tx.prepare_cached(
                    "INSERT INTO platform_turns (epoch, protx, memberships, turns, missed) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(epoch, protx) DO UPDATE SET memberships = memberships + excluded.memberships,
                        turns = turns + excluded.turns, missed = missed + excluded.missed",
                )?;
                for (id, n) in &members {
                    st.execute(params![epoch, hex(id), *n as i64, 0, 0])?;
                }
                for (id, missed) in &turns {
                    st.execute(params![epoch, hex(id), 0, 1, i64::from(*missed)])?;
                }
                tx.execute(
                    "INSERT INTO platform_turn_samples (epoch, samples, memberships) VALUES (?1, 1, ?2)
                     ON CONFLICT(epoch) DO UPDATE SET samples = samples + 1, memberships = memberships + excluded.memberships",
                    params![epoch, members.values().sum::<u64>() as i64],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await?;
    Ok((q, meta))
}

/// Seconds per Platform block: over the current epoch so far once it has some blocks, else over the
/// latest finalized epoch, else a typical value.
fn block_secs(c: &Connection, net: Network, height: u64, time: u64) -> Result<f64> {
    if let Some(now) = current(c)?
        && let (Some(first), Some(start)) = (now["first_height"].as_u64(), now["start"].as_u64())
        && height > first + 20
        && time > start
    {
        return Ok((time - start) as f64 / (height - first) as f64);
    }
    let last: Option<(i64, i64)> = c
        .query_row(
            "SELECT e.total_blocks, n.first_block_time - e.first_block_time FROM platform_epochs e
             JOIN platform_epochs n ON n.epoch = e.epoch + 1 ORDER BY e.epoch DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(match last {
        Some((blocks, secs)) if blocks > 0 && secs > 0 => secs as f64 / blocks as f64,
        _ if net == Network::Mainnet => 140.0,
        _ => 10.0,
    })
}

// ---- Platform Explorer ---------------------------------------------------------------------------

/// Platform Explorer (platform-explorer.pshenmic.dev) is a third party; it's asked about a few evonodes
/// per round, each at most daily, with a pause between requests.
const EXPLORER: &str = "https://platform-explorer.pshenmic.dev";
const EXPLORER_PER_ROUND: usize = 12;
const EXPLORER_EVERY: i64 = DAY;

fn explorer_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("dash-nodes (radar.dashhq.org)")
        .build()
        .expect("http client")
}

async fn explorer_totals(ctx: &NetCtx, client: &reqwest::Client) {
    let due: Vec<String> = match ctx
        .db
        .read(move |c| {
            Ok(query_all(
                c,
                "SELECT m.protx FROM masternodes m LEFT JOIN platform_explorer x ON x.protx = m.protx
                 WHERE m.type = 'Evo' AND m.removed IS NULL AND (x.fetched IS NULL OR x.fetched < ?1)
                 ORDER BY coalesce(x.fetched, 0) LIMIT ?2",
                params![db::now() - EXPLORER_EVERY, EXPLORER_PER_ROUND as i64],
                |r| r.get(0),
            )?)
        })
        .await
    {
        Ok(d) => d,
        Err(e) => return tracing::warn!("platform explorer: {e:#}"),
    };
    for protx in due {
        match explorer_total(client, &protx).await {
            Ok((total, epoch)) => {
                let r = ctx
                    .db
                    .write(move |c| {
                        c.execute(
                            "INSERT OR REPLACE INTO platform_explorer (protx, total_reward, epoch, fetched) VALUES (?1, ?2, ?3, ?4)",
                            params![protx, total.map(|v| v as i64), epoch.map(|v| v as i64), db::now()],
                        )?;
                        Ok(())
                    })
                    .await;
                if let Err(e) = r {
                    tracing::warn!("platform explorer: {e:#}");
                }
            }
            // Down or rate-limited: try again next round.
            Err(e) => {
                tracing::debug!("platform explorer {protx}: {e:#}");
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Its lifetime reward total for one evonode (credits; None if it doesn't know the evonode) and the
/// epoch it was in.
async fn explorer_total(client: &reqwest::Client, protx: &str) -> Result<(Option<u64>, Option<u64>)> {
    let r = client.get(format!("{EXPLORER}/validator/{}", protx.to_ascii_uppercase())).send().await?;
    if r.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok((None, None));
    }
    if !r.status().is_success() {
        bail!("HTTP {}", r.status());
    }
    let v: Value = r.json().await?;
    Ok(parse_explorer(&v))
}

pub fn parse_explorer(v: &Value) -> (Option<u64>, Option<u64>) {
    // Numbers, or numeric strings for the big ones.
    let num = |x: &Value| x.as_u64().or_else(|| x.as_str().and_then(|s| s.parse().ok()));
    (num(&v["totalReward"]), num(&v["epochInfo"]["number"]))
}

// ---- what the API serves -------------------------------------------------------------------------

/// The current epoch as the collector last read it (see `round`).
pub fn current(c: &Connection) -> Result<Option<Value>> {
    Ok(get_meta(c, NOW_KEY)?.and_then(|s| serde_json::from_str(&s).ok()))
}

fn dash(credits: i64) -> f64 {
    credits as f64 / CREDITS_PER_DASH
}

/// The value at quantile `q` of an ascending list (nearest rank).
fn quantile(sorted: &[i64], q: f64) -> Option<i64> {
    (!sorted.is_empty()).then(|| sorted[((sorted.len() - 1) as f64 * q).round() as usize])
}

/// Each finalized epoch's end, when it was paid out: the next one's start (the current epoch's for the newest).
const EPOCH_SPANS: &str = "WITH spans AS (
    SELECT epoch, first_block_time AS start,
           coalesce(lead(first_block_time) OVER (ORDER BY epoch), ?1) AS paid
    FROM platform_epochs)";

// Windows of finished epochs ("the last 30 days") start at an epoch, so the payouts are a range scan of
// their primary key. An epoch is paid out when the next one starts.

/// The first finished epoch paid out at or after `since`, if any.
fn first_paid_since(c: &Connection, current_start: i64, since: i64) -> Result<Option<i64>> {
    if current_start < since {
        return Ok(None);
    }
    let next: Option<i64> = c
        .query_row(
            "SELECT epoch FROM platform_epochs WHERE first_block_time >= ?1 ORDER BY epoch LIMIT 1",
            [since],
            |r| r.get(0),
        )
        .optional()?;
    Ok(match next {
        Some(e) => Some(e - 1),
        None => c.query_row("SELECT max(epoch) FROM platform_epochs", [], |r| r.get(0))?,
    })
}

/// Credits per evonode (`protx`, or all) over the finished epochs from `from` on.
fn earned_from(c: &Connection, from: Option<i64>, protx: Option<&str>) -> Result<HashMap<String, i64>> {
    let Some(from) = from else { return Ok(HashMap::new()) };
    let row = |r: &rusqlite::Row| Ok((r.get(0)?, r.get(1)?));
    // Two statements, so one evonode's sum walks its own index rather than every payout in the window.
    Ok(match protx {
        Some(p) => query_all(
            c,
            "SELECT protx, sum(credits) FROM platform_payouts WHERE protx = ?2 AND epoch >= ?1 GROUP BY protx",
            params![from, p],
            row,
        )?,
        None => query_all(
            c,
            "SELECT protx, sum(credits) FROM platform_payouts WHERE epoch >= ?1 GROUP BY protx",
            [from],
            row,
        )?,
    })
}

/// Platform rewards earned in the finished epochs paid out since `since`, per evonode.
fn earned_since(c: &Connection, current_start: i64, since: i64) -> Result<HashMap<String, i64>> {
    earned_from(c, first_paid_since(c, current_start, since)?, None)
}

/// Platform's yearly return on 4,000 DASH for each evonode (`protx`, or all) over the finished epochs
/// that started since `since` (credits over the epochs' own span, annualized), and that span in days.
fn yields(c: &Connection, since: i64, protx: Option<&str>) -> Result<(HashMap<String, f64>, f64)> {
    let first: Option<(i64, i64)> = c
        .query_row(
            "SELECT epoch, first_block_time FROM platform_epochs WHERE first_block_time >= ?1 ORDER BY epoch LIMIT 1",
            [since],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((epoch, start)) = first else { return Ok((HashMap::new(), 0.0)) };
    let span = current_start(c)? - start;
    if span <= 0 {
        return Ok((HashMap::new(), 0.0));
    }
    let yearly = |credits: i64| dash(credits) / span as f64 * 365.0 * DAY as f64 / EVO_COLLATERAL_DASH;
    let rows = earned_from(c, Some(epoch), protx)?;
    Ok((rows.into_iter().map(|(p, cr)| (p, yearly(cr))).collect(), span as f64 / DAY as f64))
}

/// Every evonode's Platform yield since `since`, as in `yields`.
pub fn platform_yields(c: &Connection, since: i64) -> Result<(HashMap<String, f64>, f64)> {
    yields(c, since, None)
}

/// One evonode's Platform yield since `since`; None without finished epochs in that time.
pub fn platform_yield(c: &Connection, protx: &str, since: i64) -> Result<Option<f64>> {
    let (y, span) = yields(c, since, Some(protx))?;
    Ok((span > 0.0).then(|| y.get(protx).copied().unwrap_or(0.0)))
}

fn current_start(c: &Connection) -> Result<i64> {
    Ok(current(c)?.and_then(|n| n["start"].as_i64()).unwrap_or_else(db::now))
}

/// Each evonode's monthly Platform rewards at the pace of the last six finished epochs, and their span in days.
fn monthly(c: &Connection, start: i64) -> Result<(HashMap<String, i64>, Option<f64>)> {
    let sixth: Option<(i64, i64)> = c
        .query_row(
            "SELECT epoch, first_block_time FROM platform_epochs ORDER BY epoch DESC LIMIT 1 OFFSET 5",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((epoch, from)) = sixth.filter(|(_, from)| start > *from) else { return Ok((HashMap::new(), None)) };
    let span = start - from;
    let per_month = 30.0 * DAY as f64 / span as f64;
    let rows = earned_from(c, Some(epoch), None)?;
    Ok((rows.into_iter().map(|(p, cr)| (p, (cr as f64 * per_month) as i64)).collect(), Some(span as f64 / DAY as f64)))
}

/// The pool so far in the current epoch: Core block rewards for its Core blocks up to the chain-locked
/// height (fees, about 0.2% of a pool, aren't counted).
fn pool_so_far(now: &Value) -> Option<u64> {
    let (first, to) = (now["first_core_height"].as_u64()?, now["core_height"].as_u64()?);
    Some(core_rewards(first + 1, to, CORE_HALVING))
}

/// Everything per evonode the network table and list need, keyed by ProTx.
struct Board {
    now: Option<Value>,
    last: HashMap<String, (i64, i64)>,
    d30: HashMap<String, i64>,
    monthly: HashMap<String, i64>,
    epoch_blocks: HashMap<String, i64>,
    epoch_total: i64,
    balances: HashMap<String, Option<i64>>,
    votes: HashMap<String, i64>,
}

fn board(c: &Connection) -> Result<Board> {
    let now = current(c)?;
    let start = now.as_ref().and_then(|n| n["start"].as_i64()).unwrap_or_else(db::now);
    let last_epoch: Option<i64> = c.query_row("SELECT max(epoch) FROM platform_epochs", [], |r| r.get(0))?;
    let last =
        query_all(c, "SELECT protx, blocks, credits FROM platform_payouts WHERE epoch = ?1", [last_epoch], |r| {
            Ok((r.get(0)?, (r.get(1)?, r.get(2)?)))
        })?;
    let cur_epoch = now.as_ref().and_then(|n| n["epoch"].as_i64());
    let epoch_blocks: HashMap<String, i64> =
        query_all(c, "SELECT protx, blocks FROM platform_epoch_blocks WHERE epoch = ?1", [cur_epoch], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    Ok(Board {
        last,
        d30: earned_since(c, start, db::now() - 30 * DAY)?,
        monthly: monthly(c, start)?.0,
        epoch_total: epoch_blocks.values().sum(),
        epoch_blocks,
        balances: query_all(c, "SELECT protx, credits FROM platform_balances", [], |r| Ok((r.get(0)?, r.get(1)?)))?,
        votes: query_all(c, "SELECT protx, version FROM platform_votes", [], |r| Ok((r.get(0)?, r.get(1)?)))?,
        now,
    })
}

/// The few Platform fields the masternode list carries per evonode: credits last epoch and over 30
/// days, blocks this epoch, and when it proposes next.
pub fn list_fields(c: &Connection, schedule: Option<&Schedule>) -> Result<HashMap<String, Value>> {
    let b = board(c)?;
    let ids: Vec<String> =
        query_all(c, "SELECT protx FROM masternodes WHERE type = 'Evo' AND removed IS NULL", [], |r| r.get(0))?;
    Ok(ids
        .into_iter()
        .map(|p| {
            let v = json!({
                "last_epoch": b.last.get(&p).map_or(0, |x| x.1),
                "d30": b.d30.get(&p).copied().unwrap_or(0),
                "epoch_blocks": b.epoch_blocks.get(&p).copied().unwrap_or(0),
                "next": schedule.and_then(|s| s.eta(&p)),
            });
            (p, v)
        })
        .collect())
}

/// The `/platform` document: the current epoch, finalized epochs, the upgrade vote, the proposer
/// schedule, network returns, and one row per evonode.
pub fn network_json(c: &Connection, schedule: Option<&Schedule>, net: Network) -> Result<Value> {
    let b = board(c)?;
    let Some(now) = b.now.clone() else { return Ok(json!({"now": null})) };
    let start = now["start"].as_i64().unwrap_or(0);
    let epochs = epochs_json(c, start)?;
    // Enabled evonodes: the electorate for upgrades, and the table.
    struct Evo {
        protx: String,
        service: Option<String>,
        ban: Option<i64>,
        drive: Option<String>,
        supports: Option<i64>,
        next_epoch: Option<i64>,
    }
    let evonodes: Vec<Evo> = query_all(
        c,
        "SELECT m.protx, m.service, m.pose_ban_height, s.drive, s.drive_protocol_latest, s.drive_protocol_next
         FROM masternodes m LEFT JOIN platform_status s ON s.protx = m.protx AND s.ts > ?1
         WHERE m.type = 'Evo' AND m.removed IS NULL ORDER BY m.protx",
        [db::now() - DAY],
        |r| {
            Ok(Evo {
                protx: r.get(0)?,
                service: r.get(1)?,
                ban: r.get(2)?,
                drive: r.get(3)?,
                supports: r.get(4)?,
                next_epoch: r.get(5)?,
            })
        },
    )?;
    let enabled = evonodes.iter().filter(|e| e.ban.is_none_or(|h| h <= 0)).count() as u64;
    let rows: Vec<Value> = evonodes
        .iter()
        .map(|e| {
            let p = &e.protx;
            json!({
                "protx": p, "service": e.service, "status": crate::collector::pose_status(e.ban), "drive": e.drive,
                "supports": e.supports, "vote": b.votes.get(p),
                "epoch_blocks": b.epoch_blocks.get(p).copied().unwrap_or(0),
                "last_blocks": b.last.get(p).map_or(0, |x| x.0), "last_epoch": b.last.get(p).map_or(0, |x| x.1),
                "d30": b.d30.get(p).copied().unwrap_or(0), "monthly": b.monthly.get(p).copied().unwrap_or(0),
                "balance": b.balances.get(p).copied().flatten(), "next": schedule.and_then(|s| s.eta(p)),
            })
        })
        .collect();
    // Upgrade: the version with the most votes above the current one is the candidate.
    let current_pv = now["protocol_version"].as_u64().unwrap_or(0);
    let versions: Vec<(u64, u64)> = serde_json::from_value(now["versions"].clone()).unwrap_or_default();
    let candidate = versions.iter().filter(|(v, _)| *v > current_pv).max_by_key(|(v, n)| (*n, *v)).copied();
    // Drive's own lock-in signal (getStatus next_epoch), as most freshly checked evonodes report it.
    let mut next: HashMap<i64, u64> = HashMap::new();
    for v in evonodes.iter().filter_map(|e| e.next_epoch) {
        *next.entry(v).or_default() += 1;
    }
    let reported: u64 = next.values().sum();
    let next_epoch = next.into_iter().max_by_key(|(v, n)| (*n, *v)).map(|x| x.0);
    let required = votes_required(enabled);
    let phase = match (candidate, next_epoch) {
        (_, Some(n)) if n as u64 > current_pv => "locked_in",
        (Some(_), _) => "voting",
        _ => "active",
    };
    let durations: Vec<i64> = epochs.iter().filter_map(|e| e["duration"].as_i64()).take(6).collect();
    let epoch_secs = if durations.is_empty() {
        if net == Network::Mainnet { 788_400 } else { 3_600 }
    } else {
        durations.iter().sum::<i64>() / durations.len() as i64
    };
    let blocks_so_far = b.epoch_total;
    let pool = pool_so_far(&now);
    let next_list = schedule.map(|s| {
        let mut v: Vec<(&String, &u64)> = s.slots.iter().collect();
        v.sort_by_key(|x| *x.1);
        v.into_iter().take(30).map(|(p, slot)| json!({"protx": p, "slots": slot, "eta": s.eta(p)})).collect::<Vec<_>>()
    });
    Ok(json!({
        "now": {
            "epoch": now["epoch"], "start": start, "ends": start + epoch_secs, "epoch_secs": epoch_secs,
            "height": now["height"], "first_height": now["first_height"], "core_height": now["core_height"],
            "protocol_version": current_pv, "blocks": blocks_so_far, "proposers": b.epoch_blocks.len(),
            "pool_so_far": pool, "updated": now["updated"],
        },
        "upgrade": {
            "current": current_pv, "candidate": candidate.map(|c| c.0), "votes": candidate.map_or(0, |c| c.1),
            "required": required, "enabled_evonodes": enabled, "versions": versions, "phase": phase,
            "next_epoch": next_epoch, "next_epoch_reported_by": reported,
        },
        "returns": network_returns(c, start)?,
        "epochs": epochs,
        "schedule": schedule.map(|s| json!({"ts": s.ts, "height": s.height, "epoch": s.epoch, "block_secs": s.block_secs, "next": next_list})),
        "evonodes": rows,
    }))
}

/// Finalized epochs, newest first, with their pool and how blocks and rewards spread over proposers.
fn epochs_json(c: &Connection, current_start: i64) -> Result<Vec<Value>> {
    let mut rows: Vec<Value> = query_all(
        c,
        &format!(
            "{EPOCH_SPANS} SELECT e.epoch, e.first_block_time, s.paid, e.total_blocks, e.proposers,
                e.processing_fees + e.storage_fees, e.core_rewards, e.protocol_version, e.first_block_height
             FROM platform_epochs e JOIN spans s ON s.epoch = e.epoch ORDER BY e.epoch DESC LIMIT ?2"
        ),
        params![current_start, EPOCHS_SHOWN],
        |r| {
            let (start, paid, blocks, fees, core): (i64, i64, i64, i64, i64) =
                (r.get(1)?, r.get(2)?, r.get(3)?, r.get(5)?, r.get(6)?);
            let pool = fees + core;
            Ok(json!({
                "epoch": r.get::<_, i64>(0)?, "start": start, "end": paid, "duration": paid - start,
                "first_height": r.get::<_, i64>(8)?, "blocks": blocks, "proposers": r.get::<_, i64>(4)?, "pool": pool,
                "core_rewards": core, "fees": fees, "per_block": if blocks > 0 { pool / blocks } else { 0 },
                "protocol_version": r.get::<_, i64>(7)?,
            }))
        },
    )?;
    let mut per: HashMap<i64, (Vec<i64>, Vec<i64>)> = HashMap::new();
    let mut st = c.prepare("SELECT epoch, blocks, credits FROM platform_payouts WHERE epoch >= ?1")?;
    let mut q = st.query([rows.last().and_then(|r| r["epoch"].as_i64()).unwrap_or(0)])?;
    while let Some(r) = q.next()? {
        let e = per.entry(r.get(0)?).or_default();
        e.0.push(r.get(1)?);
        e.1.push(r.get(2)?);
    }
    let spread = |v: &[i64]| json!({"p10": quantile(v, 0.1), "p50": quantile(v, 0.5), "p90": quantile(v, 0.9), "min": v.first(), "max": v.last()});
    for row in &mut rows {
        let (mut b, mut cr) = row["epoch"].as_i64().and_then(|e| per.remove(&e)).unwrap_or_default();
        b.sort_unstable();
        cr.sort_unstable();
        row["blocks_spread"] = spread(&b);
        row["credits_spread"] = spread(&cr);
    }
    Ok(rows)
}

/// Median evonode returns: Platform rewards per epoch and per month, and the yearly yield from Platform
/// over the last 90 days (Core's comes from payments, in the summary).
fn network_returns(c: &Connection, start: i64) -> Result<Value> {
    let (monthly, basis) = monthly(c, start)?;
    let mut m: Vec<i64> = monthly.into_values().collect();
    m.sort_unstable();
    let (yields, span) = platform_yields(c, db::now() - 90 * DAY)?;
    let mut y: Vec<f64> = yields.into_values().collect();
    y.sort_by(f64::total_cmp);
    Ok(json!({
        "median_monthly": quantile(&m, 0.5), "monthly_basis_days": basis, "evonodes_paid": m.len(),
        "median_platform_yield": (!y.is_empty()).then(|| (y[y.len() / 2] * 1e4).round() / 1e4), "yield_span_days": span,
    }))
}

/// An evonode's Platform rewards for its page.
pub fn mn_json(c: &Connection, protx: &str, schedule: Option<&Schedule>, window_days: i64) -> Result<Value> {
    let now = current(c)?;
    let start = now.as_ref().and_then(|n| n["start"].as_i64()).unwrap_or_else(db::now);
    let max_epoch: Option<i64> = c.query_row("SELECT max(epoch) FROM platform_epochs", [], |r| r.get(0))?;
    let Some(max_epoch) = max_epoch else { return Ok(Value::Null) };
    let from = max_epoch - EPOCHS_SHOWN + 1;
    // Rank by credits among the epoch's proposers, and the epoch's spread.
    let epochs: Vec<Value> = query_all(
        c,
        &format!(
            "{EPOCH_SPANS}, ranked AS (
                SELECT epoch, protx, blocks, credits, rank() OVER (PARTITION BY epoch ORDER BY credits DESC) AS rank,
                       count(*) OVER (PARTITION BY epoch) AS n
                FROM platform_payouts WHERE epoch >= ?2)
             SELECT e.epoch, e.first_block_time, s.paid, e.total_blocks, coalesce(r.blocks, 0), coalesce(r.credits, 0), r.rank, e.proposers
             FROM platform_epochs e JOIN spans s ON s.epoch = e.epoch LEFT JOIN ranked r ON r.epoch = e.epoch AND r.protx = ?3
             WHERE e.epoch >= ?2 ORDER BY e.epoch DESC"
        ),
        params![start, from, protx],
        |r| {
            let total: i64 = r.get(3)?;
            let blocks: i64 = r.get(4)?;
            Ok(json!({
                "epoch": r.get::<_, i64>(0)?, "start": r.get::<_, i64>(1)?, "end": r.get::<_, i64>(2)?, "blocks": blocks,
                "credits": r.get::<_, i64>(5)?, "share": if total > 0 { blocks as f64 / total as f64 } else { 0.0 },
                "rank": r.get::<_, Option<i64>>(6)?, "of": r.get::<_, i64>(7)?,
            }))
        },
    )?;
    // Drop the epochs before it first proposed: it wasn't an evonode yet, or not in a validator set.
    let first_paid: Option<i64> =
        c.query_row("SELECT min(epoch) FROM platform_payouts WHERE protx = ?1", [protx], |r| r.get(0))?;
    let epochs: Vec<Value> = match first_paid {
        Some(f) => epochs.into_iter().filter(|e| e["epoch"].as_i64() >= Some(f)).collect(),
        None => Vec::new(),
    };
    let sum_since = |days: i64| -> Result<i64> {
        let from = first_paid_since(c, start, db::now() - days * DAY)?;
        Ok(earned_from(c, from, Some(protx))?.get(protx).copied().unwrap_or(0))
    };
    let (lifetime, paid_epochs): (i64, i64) = c.query_row(
        "SELECT coalesce(sum(credits), 0), count(*) FROM platform_payouts WHERE protx = ?1 AND credits > 0",
        [protx],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let first_epoch: Option<i64> = c.query_row("SELECT min(epoch) FROM platform_epochs", [], |r| r.get(0))?;
    // This node among every enabled evonode (zeros count) over the last 30 days.
    let d30 = earned_since(c, start, db::now() - 30 * DAY)?;
    let enabled: Vec<String> = query_all(
        c,
        "SELECT protx FROM masternodes WHERE type = 'Evo' AND removed IS NULL AND coalesce(pose_ban_height, -1) <= 0",
        [],
        |r| r.get(0),
    )?;
    let mine30 = d30.get(protx).copied().unwrap_or(0);
    let mut all30: Vec<i64> = enabled.iter().map(|p| d30.get(p).copied().unwrap_or(0)).collect();
    all30.sort_unstable();
    let below = all30.iter().filter(|&&x| x < mine30).count();
    let (monthly_map, basis) = monthly(c, start)?;
    // The current epoch so far.
    let cur_epoch = now.as_ref().and_then(|n| n["epoch"].as_i64());
    let blocks: Vec<(String, i64)> =
        query_all(c, "SELECT protx, blocks FROM platform_epoch_blocks WHERE epoch = ?1", [cur_epoch], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    let total: i64 = blocks.iter().map(|b| b.1).sum();
    let mine = blocks.iter().find(|b| b.0 == protx).map_or(0, |b| b.1);
    let rank = blocks.iter().filter(|b| b.1 > mine).count() + 1;
    let pool = now.as_ref().and_then(pool_so_far);
    let accrued = pool.filter(|_| total > 0).map(|p| (u128::from(p) * mine as u128 / total as u128) as i64);
    // Turns we watched this epoch and over finalized epochs of the last 30 days.
    let turns = |from_epoch: i64, to_epoch: i64| -> Result<Value> {
        let (mem, turns, missed): (i64, i64, i64) = c.query_row(
            "SELECT coalesce(sum(memberships), 0), coalesce(sum(turns), 0), coalesce(sum(missed), 0) FROM platform_turns
             WHERE protx = ?1 AND epoch BETWEEN ?2 AND ?3",
            params![protx, from_epoch, to_epoch],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let all: i64 = c.query_row(
            "SELECT coalesce(sum(memberships), 0) FROM platform_turn_samples WHERE epoch BETWEEN ?1 AND ?2",
            params![from_epoch, to_epoch],
            |r| r.get(0),
        )?;
        Ok(json!({"turns": turns, "missed": missed, "expected_share": (all > 0).then(|| mem as f64 / all as f64)}))
    };
    let current = match cur_epoch {
        Some(e) => {
            let mut t = turns(e, e)?;
            if let Some(share) = t["expected_share"].as_f64() {
                t["expected_blocks"] = json!((share * total as f64 * 10.0).round() / 10.0);
            }
            json!({
                "epoch": e, "blocks": mine, "blocks_total": total, "proposers": blocks.len(), "rank": (mine > 0).then_some(rank),
                "accrued": accrued, "pool_so_far": pool, "turns": t,
            })
        }
        None => Value::Null,
    };
    // Finalized epochs we watched in full enough to say how many blocks it should have made.
    let watched: Vec<Value> = query_all(
        c,
        "SELECT t.epoch, t.memberships, s.memberships, e.total_blocks, coalesce(p.blocks, 0), t.turns, t.missed, s.samples,
                n.first_block_time - e.first_block_time
         FROM platform_turns t JOIN platform_turn_samples s ON s.epoch = t.epoch JOIN platform_epochs e ON e.epoch = t.epoch
         JOIN platform_epochs n ON n.epoch = t.epoch + 1
         LEFT JOIN platform_payouts p ON p.epoch = t.epoch AND p.protx = t.protx
         WHERE t.protx = ?1 ORDER BY t.epoch DESC LIMIT 12",
        [protx],
        |r| {
            let (mem, all, total, actual): (i64, i64, i64, i64) = (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?);
            let (samples, secs): (i64, i64) = (r.get(7)?, r.get(8)?);
            let coverage = samples as f64 * QUORUM_INTERVAL.as_secs_f64() / secs.max(1) as f64;
            Ok(json!({
                "epoch": r.get::<_, i64>(0)?, "expected": (all > 0).then(|| (mem as f64 / all as f64 * total as f64 * 10.0).round() / 10.0),
                "actual": actual, "turns": r.get::<_, i64>(5)?, "missed": r.get::<_, i64>(6)?,
                "coverage": (coverage.min(1.0) * 100.0).round() / 100.0,
            }))
        },
    )?;
    let last30 = cur_epoch.map(|e| e - 30 * DAY / epoch_secs_hint(c, &now).max(1)).unwrap_or(0);
    let balance: Option<(Option<i64>, i64)> = c
        .query_row("SELECT credits, ts FROM platform_balances WHERE protx = ?1", [protx], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    let history: Vec<Value> = query_all(
        c,
        "SELECT ts, credits FROM platform_balance_history WHERE protx = ?1 ORDER BY ts DESC LIMIT 60",
        [protx],
        |r| Ok(json!({"ts": r.get::<_, i64>(0)?, "credits": r.get::<_, i64>(1)?})),
    )?;
    let vote: Option<(i64, i64)> = c
        .query_row("SELECT version, ts FROM platform_votes WHERE protx = ?1", [protx], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let explorer: Option<(Option<i64>, Option<i64>, i64)> = c
        .query_row("SELECT total_reward, epoch, fetched FROM platform_explorer WHERE protx = ?1", [protx], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?;
    // Our own total over the epochs Platform Explorer had counted when asked: what it says beyond that
    // is from before epoch 34.
    let explorer = match explorer {
        Some((total, epoch, fetched)) => {
            let ours: Option<i64> = match epoch {
                Some(e) => c.query_row(
                    "SELECT sum(credits) FROM platform_payouts WHERE protx = ?1 AND epoch < ?2",
                    params![protx, e],
                    |r| r.get(0),
                )?,
                None => None,
            };
            json!({"total_reward": total, "epoch": epoch, "fetched": fetched, "ours_to_then": ours.unwrap_or(0),
                "before_ours": total.map(|t| (t - ours.unwrap_or(0)).max(0))})
        }
        None => Value::Null,
    };
    Ok(json!({
        "first_epoch": first_epoch,
        "epochs": epochs,
        "totals": {
            "last_epoch": c.query_row("SELECT credits FROM platform_payouts WHERE protx = ?1 AND epoch = ?2", params![protx, max_epoch], |r| r.get::<_, i64>(0)).optional()?.unwrap_or(0),
            "last_epoch_index": max_epoch,
            "d30": sum_since(30)?, "d90": sum_since(90)?, "d365": sum_since(365)?, "lifetime": lifetime, "epochs_paid": paid_epochs,
        },
        "d30_percentile": (!all30.is_empty()).then(|| (below as f64 / all30.len() as f64 * 1000.0).round() / 1000.0),
        "d30_of": all30.len(),
        "monthly": monthly_map.get(protx).copied().unwrap_or(0), "monthly_basis_days": basis,
        "annual_yield": platform_yield(c, protx, db::now() - window_days * DAY)?.map(|y| (y * 1e4).round() / 1e4),
        "current": current,
        "watched": watched,
        "turns_30d": turns(last30, cur_epoch.unwrap_or(0))?,
        "next": schedule.and_then(|s| s.slots.get(protx).map(|slot| json!({"slots": slot, "eta": s.eta(protx), "sampled": s.ts}))),
        "in_validator_sets": schedule.map(|s| s.slots.contains_key(protx)),
        "balance": balance.map(|(credits, ts)| json!({"credits": credits, "ts": ts})),
        "balance_history": history,
        "vote": vote.map(|(v, ts)| json!({"version": v, "ts": ts})),
        "explorer": explorer,
    }))
}

/// Seconds per epoch, for turning "30 days" into epochs: the latest finalized one's, or the network's.
fn epoch_secs_hint(c: &Connection, now: &Option<Value>) -> i64 {
    let last: Option<i64> = c
        .query_row(
            "SELECT n.first_block_time - e.first_block_time FROM platform_epochs e JOIN platform_epochs n ON n.epoch = e.epoch + 1
             ORDER BY e.epoch DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();
    last.or_else(|| {
        let n = now.as_ref()?;
        Some(n["time"].as_i64()? - n["start"].as_i64()?)
    })
    .filter(|s| *s > 0)
    .unwrap_or(788_400)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(b: u8) -> [u8; 32] {
        [b; 32]
    }

    #[test]
    fn payouts_follow_drive_integer_rules() {
        // 1000 credits over 3 + 3 + 1 blocks: 428, 428, 142 rounded down, and the 2 left go to the last
        // proposer in identifier order, whatever order they came in.
        let p = payouts(1000, 7, &[(id(3), 1), (id(1), 3), (id(2), 3)]);
        assert_eq!(p, vec![(id(1), 3, 428), (id(2), 3, 428), (id(3), 1, 144)]);
        assert_eq!(p.iter().map(|x| x.2).sum::<u64>(), 1000);
        // Real sizes don't overflow: a 2,300 DASH pool, 25 of 5,573 blocks.
        let pool = 230_000_000_000_000u64;
        let p = payouts(pool, 5_573, &[(id(1), 25), (id(2), 5_548)]);
        assert_eq!(p[0].2, pool * 25 / 5_573);
        assert_eq!(p[0].2 + p[1].2, pool);
        assert!(payouts(1000, 0, &[(id(1), 0)]).iter().all(|x| x.2 == 1000));
        assert!(payouts(1000, 5, &[]).is_empty());
    }

    #[test]
    fn core_rewards_match_drive() {
        // rs-dpp's own cases (halving interval 150): the first year at the genesis rate, and a span
        // across a halving.
        assert_eq!(core_rewards(1, 150, 150), 112_500_000_000 * 150);
        let second = 112_500_000_000 - 112_500_000_000 / 14;
        assert_eq!(core_rewards(150, 155, 150), 112_500_000_000 + 5 * second);
        assert_eq!(core_rewards(10, 9, 150), 0);
        // Mainnet, block 2,563,677 (year 12): 0.46231323 DASH a block went into the credit pool.
        assert_eq!(core_rewards(2_563_677, 2_563_677, CORE_HALVING) / 1000, 46_231_323);
    }

    #[test]
    fn upgrade_threshold() {
        assert_eq!(votes_required(360), 242);
        assert_eq!(votes_required(100), 68);
        assert_eq!(votes_required(0), 1);
    }

    fn set(hash: u8, members: &[(u8, bool)]) -> ValidatorSet {
        ValidatorSet {
            quorum_hash: vec![hash],
            members: members.iter().map(|&(m, banned)| Member { pro_tx_hash: id(m), banned }).collect(),
        }
    }

    #[test]
    fn schedule_walks_the_rest_of_the_set_then_the_next() {
        let q = Quorums {
            order: vec![vec![1], vec![2], vec![3]],
            current: vec![2],
            sets: vec![
                set(1, &[(1, false), (9, false)]),
                set(2, &[(3, false), (4, true), (5, false), (6, false)]),
                set(3, &[(7, false), (5, false)]),
            ],
            last_proposer: id(3),
        };
        // 5 and 6 finish set 2 (4 is banned), then set 3 (5 again: only its first turn counts), then set 1.
        assert_eq!(schedule(&q), vec![(id(5), 1), (id(6), 2), (id(7), 3), (id(1), 5), (id(9), 6)]);
    }

    #[test]
    fn schedule_skips_the_two_oldest_of_many_sets() {
        let order: Vec<Vec<u8>> = (0..12).map(|i| vec![i]).collect();
        let sets: Vec<ValidatorSet> = (0..12).map(|i| set(i, &[(i + 100, false)])).collect();
        let q = Quorums { order, current: vec![9], sets, last_proposer: id(109) };
        let s = schedule(&q);
        // From set 9 (the last rotatable one) back to 0 … 8; sets 10 and 11 never get a turn.
        assert_eq!(s.first(), Some(&(id(100), 1)));
        assert_eq!(s.len(), 9);
        assert!(!s.iter().any(|(m, _)| *m == id(110) || *m == id(111)));
        assert_eq!(memberships(&q).len(), 10);
    }

    #[test]
    fn turns_are_pinned_only_when_certain() {
        let s = set(1, &[(1, false), (2, false), (3, true), (4, false), (5, false)]);
        // 1 → 4 in two blocks: 2 and 4 proposed (3 is banned, so it had no turn).
        assert_eq!(turns_between(&s, &id(1), &id(4), 2), Some(vec![(id(2), false), (id(4), false)]));
        // 1 → 5 in one block: 2 and 4 missed.
        assert_eq!(turns_between(&s, &id(1), &id(5), 1), Some(vec![(id(2), true), (id(4), true), (id(5), false)]));
        // 1 → 5 in two blocks: one of 2 and 4 missed, but which?
        assert_eq!(turns_between(&s, &id(1), &id(5), 2), None);
        // Backwards, or no new block: nothing to say.
        assert_eq!(turns_between(&s, &id(4), &id(2), 1), None);
        assert_eq!(turns_between(&s, &id(2), &id(2), 0), None);
    }

    #[test]
    fn requests_encode_like_protobuf() {
        let mut r = Vec::new();
        put_uint(&mut r, 1, 34);
        put_uint(&mut r, 3, 300);
        assert_eq!(r, [0x08, 34, 0x18, 0xac, 0x02]);
        assert_eq!(v0(&r), [0x0a, 5, 0x08, 34, 0x18, 0xac, 0x02]);
        assert_eq!(v0(&[]), [0x0a, 0x00]);
        assert_eq!(unhex(&hex(&id(0xab))), Some(id(0xab)));
        assert_eq!(unhex("zz"), None);
    }

    /// Answers recorded from a mainnet evonode on 2026-10-31 (src/testdata), unframed.
    fn recorded(bytes: &[u8]) -> Vec<u8> {
        platform::first_data_frame(bytes).expect("a data frame").to_vec()
    }

    fn display_id(s: &str) -> [u8; 32] {
        unhex(s).unwrap()
    }

    #[test]
    fn decodes_recorded_finalized_epoch() {
        let (epochs, meta) = parse_finalized(&recorded(include_bytes!("testdata/finalized-86.bin"))).unwrap();
        assert_eq!(meta.epoch, 87);
        let [e] = epochs.as_slice() else { panic!("one epoch") };
        assert_eq!((e.epoch, e.total_blocks, e.proposers.len()), (86, 5_573, 360));
        assert_eq!(
            (e.processing_fees, e.storage_fees, e.core_rewards),
            (508_444_592_700, 3_220_268_823, 231_202_846_443_024)
        );
        assert_eq!(e.proposers.iter().map(|p| p.1).sum::<u64>(), e.total_blocks);
        let paid = payouts(e.pool(), e.total_blocks, &e.proposers);
        assert_eq!(paid.iter().map(|p| p.2).sum::<u64>(), e.pool());
        // Proposer ids are the ProTx hash as displayed. This one's identity balance was checked
        // against its payouts and withdrawals (RESEARCH.md §2b).
        let be6d = paid
            .iter()
            .find(|p| p.0 == display_id("be6d577dcf32d552d3bba8d6b8f68c144e7b644dde0653a49b04bb163afb15da"))
            .unwrap();
        assert_eq!((be6d.1, be6d.2), (14, 582_092_797_104));
        // The last in identifier order, with as many blocks, also gets the 219 credits the divisions left.
        let last = paid.last().unwrap();
        assert_eq!(
            (hex(&last.0).as_str(), last.1, last.2),
            ("ff6b1f87f033feb9c2b0c8bb14d4d626f9cd660357d43d201dd4ec918da61cad", 14, 582_092_797_323)
        );
    }

    #[test]
    fn decodes_recorded_live_answers() {
        let (epochs, meta) = parse_epochs(&recorded(include_bytes!("testdata/epochs-current.bin"))).unwrap();
        assert_eq!(epochs[0].epoch, 87);
        assert_eq!(epochs[0].start_ms / 1000, 1_793_386_337);
        assert_eq!((epochs[0].first_block_height, epochs[0].first_core_height), (461_783, 2_564_064));
        assert!(meta.height > epochs[0].first_block_height && meta.core_height > epochs[0].first_core_height);
        let (blocks, _) = parse_pairs(&recorded(include_bytes!("testdata/blocks-page.bin"))).unwrap();
        assert_eq!(blocks.len(), 5);
        assert!(blocks.windows(2).all(|w| w[0].0 < w[1].0) && blocks.iter().all(|b| b.1 > Some(0)));
        let (balances, _) = parse_pairs(&recorded(include_bytes!("testdata/balances.bin"))).unwrap();
        let be6d = display_id("be6d577dcf32d552d3bba8d6b8f68c144e7b644dde0653a49b04bb163afb15da");
        assert_eq!(balances.iter().find(|b| b.0 == be6d).map(|b| b.1), Some(Some(2_392_835_974_656)));
        let (versions, meta) = parse_upgrade_state(&recorded(include_bytes!("testdata/upgrade-state.bin"))).unwrap();
        assert_eq!(meta.protocol_version, 13);
        assert!(versions.iter().any(|&(v, n)| v == 14 && n >= votes_required(360)));
        let (votes, _) = parse_pairs(&recorded(include_bytes!("testdata/votes-page.bin"))).unwrap();
        assert_eq!(votes.len(), 5);
        assert!(votes.iter().all(|v| matches!(v.1, Some(13 | 14))));
    }

    #[test]
    fn decodes_recorded_validator_sets() {
        use std::io::Read;
        let mut raw = Vec::new();
        flate2::read::GzDecoder::new(&include_bytes!("testdata/quorums.bin.gz")[..]).read_to_end(&mut raw).unwrap();
        let (q, meta) = parse_quorums(&recorded(&raw)).unwrap();
        assert_eq!(meta.epoch, 87);
        assert_eq!(q.order.len(), q.sets.len());
        assert!(q.sets.iter().all(|s| s.members.len() == 100));
        let current = q.sets.iter().find(|s| s.quorum_hash == q.current).expect("the active set");
        // The last proposer is one of the active set, and ids are as displayed (this one is a mainnet evonode).
        assert!(current.members.iter().any(|m| m.pro_tx_hash == q.last_proposer));
        assert!(q.sets.iter().any(|s| {
            s.members
                .iter()
                .any(|m| hex(&m.pro_tx_hash) == "be6d577dcf32d552d3bba8d6b8f68c144e7b644dde0653a49b04bb163afb15da")
        }));
        let s = schedule(&q);
        assert!(s[0].0 > q.last_proposer && s[0].1 == 1);
        // Every slot is someone's first turn, in order, and nobody twice.
        assert!(s.windows(2).all(|w| w[0].1 < w[1].1));
        assert_eq!(s.iter().map(|x| x.0).collect::<HashSet<_>>().len(), s.len());
        assert!(s.len() > 300);
    }

    #[test]
    fn explorer_fields() {
        let v = json!({"totalReward": 36_235_244_983_446u64, "epochInfo": {"number": 87}});
        assert_eq!(parse_explorer(&v), (Some(36_235_244_983_446), Some(87)));
        assert_eq!(parse_explorer(&json!({"totalReward": "12", "epochInfo": null})), (Some(12), None));
        assert_eq!(parse_explorer(&json!({"message": "not found"})), (None, None));
    }
}

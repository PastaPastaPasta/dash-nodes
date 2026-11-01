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
//! counts only once two evonodes at different IPs give the same one, at the same chain state. Finalized
//! epochs never change, so each is stored once: three evonodes have to agree on it, and it's checked
//! against Drive's own rules (`check_finalized`): numbered as its start time says, starting where the
//! one before ended, and the last ending where the current epoch starts. Core heights an answer
//! claims are held to our dashd's tip. If finalized epochs stop moving on all the same, the newest
//! stored ones are asked for again (`recover_tail`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rand::seq::SliceRandom;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::api::{median4, round_to, round4};
use crate::db::{self, get_meta, query_all, set_meta};
use crate::network::Network;
use crate::platform::{self, Field, fields, msg, uint};
use crate::state::NetCtx;

const POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// A mainnet block takes about 140 s: sampling more often sees nearly every proposer turn.
const QUORUM_INTERVAL: Duration = Duration::from_secs(90);
/// Most items Drive returns per query (`max_returned_elements`, and the ID limit of getIdentitiesBalances).
const PAGE: usize = 100;
/// Largest answers read, per kind: 100 finalized epochs of ~360 proposers are about 1.4 MB, the validator
/// sets (24 of 100 members) about 126 KB, and a page of 100 pairs or the current epoch a few KB.
const FINALIZED_ANSWER: usize = 4 << 20;
const QUORUMS_ANSWER: usize = 1 << 20;
const SMALL_ANSWER: usize = 256 << 10;
/// Pages of 100 read at most per paged query: more than any network has evonodes.
const MAX_PAGES: usize = 20;
/// Pages of finalized epochs read per round at most, the rest next round: a made-up current epoch far
/// ahead can't have one round ask for every epoch up to it.
const FINALIZED_PAGES: usize = 100;
/// How far apart (in Platform or Core blocks) two answers' chain heights may be to count as one state.
const AGREE_BLOCKS: u64 = 2;
/// Clock skew allowed for times an answer claims.
const SKEW_SECS: u64 = 300;
/// Evonodes asked before giving up on two matching answers.
const MAX_ASKED: usize = 6;
/// Epoch lists published per masternode and in the network document (a testnet epoch is an hour).
const EPOCHS_SHOWN: i64 = 120;
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
    let mut out = Vec::new();
    for f in fields(buf) {
        if let (n, Field::Bytes(b)) = f?
            && n == num
        {
            out.push(b);
        }
    }
    Ok(out)
}

fn id32(b: &[u8]) -> Result<[u8; 32]> {
    b.try_into().map_err(|_| anyhow!("identifier of {} bytes", b.len()))
}

fn hex(b: &[u8]) -> String {
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
struct Meta {
    height: u64,
    core_height: u64,
    epoch: u64,
    time_ms: u64,
    protocol_version: u64,
}

/// Two answers' metadata if they describe one chain state: the same epoch and protocol version, and
/// Platform and Core heights at most AGREE_BLOCKS apart. Heights and time are the lower of the two, so
/// neither evonode alone can move them ahead.
fn agreed_meta(a: &Meta, b: &Meta) -> Option<Meta> {
    let close = |x: u64, y: u64| x.abs_diff(y) <= AGREE_BLOCKS;
    (a.epoch == b.epoch
        && a.protocol_version == b.protocol_version
        && close(a.height, b.height)
        && close(a.core_height, b.core_height))
    .then(|| Meta {
        height: a.height.min(b.height),
        core_height: a.core_height.min(b.core_height),
        time_ms: a.time_ms.min(b.time_ms),
        ..*a
    })
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
struct Finalized {
    epoch: u64,
    first_block_height: u64,
    first_core_height: u64,
    first_block_time_ms: u64,
    protocol_version: u64,
    total_blocks: u64,
    next_core_height: u64,
    processing_fees: u64,
    storage_fees: u64,
    core_rewards: u64,
    /// (identity id = ProTx hash as displayed, blocks proposed).
    proposers: Vec<([u8; 32], u64)>,
}

impl Finalized {
    /// What Drive paid out for the epoch: processing fees, the epoch's share of storage fees, and
    /// the Core block rewards for its Core blocks.
    fn pool(&self) -> u64 {
        self.processing_fees + self.storage_fees + self.core_rewards
    }
}

/// GetFinalizedEpochInfosResponse: v0 { epochs = 1 { finalized_epoch_infos = 1 [ { number = 1,
/// first_block_height = 2, first_core_block_height = 3, first_block_time = 4, protocol_version = 6,
/// total_blocks_in_epoch = 7, next_epoch_start_core_block_height = 8, total_processing_fees = 9,
/// total_distributed_storage_fees = 10, core_block_rewards = 12, block_proposers = 13 [ { proposer_id
/// = 1, block_count = 2 } ] } ] }, metadata = 3 }.
fn parse_finalized(resp: &[u8]) -> Result<(Vec<Finalized>, Meta)> {
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
struct Epoch {
    epoch: u64,
    first_block_height: u64,
    first_core_height: u64,
    start_ms: u64,
    protocol_version: u64,
}

/// GetEpochsInfoResponse: v0 { epochs = 1 { epoch_infos = 1 [ { number = 1, first_block_height = 2,
/// first_core_block_height = 3, start_time = 4, protocol_version = 6 } ] }, metadata = 3 }.
fn parse_epochs(resp: &[u8]) -> Result<(Vec<Epoch>, Meta)> {
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

type Pairs = Vec<([u8; 32], Option<u64>)>;

/// A list of (32-byte id, number) pairs, the shape of several answers: GetEvonodesProposedEpochBlocksResponse
/// (v0 { evonodes_proposed_block_counts_info = 1 { evonodes_proposed_block_counts = 1 [ { pro_tx_hash = 1,
/// count = 2 } ] } }), GetIdentitiesBalancesResponse (v0 { identities_balances = 1 { entries = 1 [ {
/// identity_id = 1, balance = 2 } ] } }; an identity without a balance has no field 2) and
/// GetProtocolVersionUpgradeVoteStatusResponse (v0 { versions = 1 { version_signals = 1 [ {
/// pro_tx_hash = 1, version = 2 } ] } }).
fn parse_pairs(resp: &[u8]) -> Result<(Pairs, Meta)> {
    let (_, list, meta) = result(resp)?;
    let mut out = Vec::new();
    for e in all(list, 1)? {
        out.push((id32(msg(e, 1).unwrap_or_default())?, uint(e, 2)));
    }
    Ok((out, meta))
}

/// GetProtocolVersionUpgradeStateResponse: v0 { versions = 1 { versions = 1 [ { version_number = 1,
/// vote_count = 2 } ] }, metadata = 3 }. (version, votes), in the answer's order.
fn parse_upgrade_state(resp: &[u8]) -> Result<(Vec<(u64, u64)>, Meta)> {
    let (_, list, meta) = result(resp)?;
    let out = all(list, 1)?.into_iter().map(|e| (uint(e, 1).unwrap_or(0), uint(e, 2).unwrap_or(0))).collect();
    Ok((out, meta))
}

#[derive(Clone, Debug, PartialEq)]
struct Member {
    /// As displayed, like every other id here: Drive's validator sets are keyed and ordered by these bytes.
    pro_tx_hash: [u8; 32],
    banned: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct ValidatorSet {
    quorum_hash: Vec<u8>,
    /// Sorted by `pro_tx_hash`: Tenderdash's proposer order.
    members: Vec<Member>,
}

#[derive(Clone, Debug, PartialEq)]
struct Quorums {
    /// Every validator set's quorum hash, in Drive's rotation order.
    order: Vec<Vec<u8>>,
    current: Vec<u8>,
    sets: Vec<ValidatorSet>,
    last_proposer: [u8; 32],
}

/// Most validator sets, and members per set, a sample may have (mainnet has 24 of 100): the schedule
/// walks every member of every set, so one evonode's answer can't make that billions of steps.
const MAX_SETS: usize = 100;
const MAX_MEMBERS: usize = 1_000;

/// GetCurrentQuorumsInfoResponse: v0 { quorum_hashes = 1 [], current_quorum_hash = 2, validator_sets = 3
/// [ { quorum_hash = 1, members = 3 [ { pro_tx_hash = 1, is_banned = 3 } ] } ], last_block_proposer = 4,
/// metadata = 5 }. The rotation order is kept without repeats.
fn parse_quorums(resp: &[u8]) -> Result<(Quorums, Meta)> {
    let v0 = msg(resp, 1).ok_or_else(|| anyhow!("no v0 answer"))?;
    let mut sets = Vec::new();
    for s in all(v0, 3)? {
        if sets.len() == MAX_SETS {
            bail!("more than {MAX_SETS} validator sets");
        }
        let mut members = Vec::new();
        for m in all(s, 3)? {
            if members.len() == MAX_MEMBERS {
                bail!("a validator set of more than {MAX_MEMBERS} members");
            }
            members.push(Member { pro_tx_hash: id32(msg(m, 1).unwrap_or_default())?, banned: uint(m, 3) == Some(1) });
        }
        members.sort_by_key(|m| m.pro_tx_hash);
        sets.push(ValidatorSet { quorum_hash: msg(s, 1).unwrap_or_default().to_vec(), members });
    }
    let mut seen = HashSet::new();
    let order: Vec<Vec<u8>> = all(v0, 1)?.into_iter().filter(|h| seen.insert(*h)).map(<[u8]>::to_vec).collect();
    if order.len() > MAX_SETS {
        bail!("more than {MAX_SETS} validator sets in the rotation");
    }
    let q = Quorums {
        order,
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
/// mainnet) aren't deducted, so this is the evonode's gross. A proposer without an identity balance
/// isn't paid at all (Drive moves its share to the current epoch's pool); every evonode gets a funded
/// identity when it registers, so that isn't checked here.
fn payouts(pool: u64, total_blocks: u64, proposers: &[([u8; 32], u64)]) -> Vec<([u8; 32], u64, u64)> {
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
/// starting at 5 DASH and cut by 1/14 every `halving` blocks. None where Drive has no answer either:
/// heights past u32, years past its 200-year table (CORE_HALVING_DISTRIBUTION), or an overflow.
fn core_rewards(from: u64, to: u64, halving: u64) -> Option<u64> {
    const GENESIS: u64 = 112_500_000_000;
    const YEARS: u64 = 200;
    if from > to || halving == 0 {
        return Some(0);
    }
    if to > u64::from(u32::MAX) {
        return None;
    }
    let per_block = |year: u64| (0..year).fold(GENESIS, |d, _| d - d / 14);
    let (prev_from, prev_to) = (from.saturating_sub(1), to.saturating_sub(1));
    let (first_year, last_year) = (prev_from / halving, prev_to / halving);
    if last_year >= YEARS {
        return None;
    }
    (first_year..=last_year).try_fold(0u64, |sum, y| {
        let start = if y == first_year { prev_from } else { y * halving };
        let end = if y == last_year { to } else { (y + 1) * halving };
        sum.checked_add((end - start).checked_mul(per_block(y))?)
    })
}

/// Core's subsidy halving interval (the same on mainnet and testnet).
const CORE_HALVING: u64 = 210_240;

/// Votes an upgrade needs at the epoch change: `1 + floor(active × 67 / 100)` (check_for_desired_protocol_upgrade).
fn votes_required(active_evonodes: u64) -> u64 {
    1 + active_evonodes * 67 / 100
}

/// The turn order from a validator-set sample, mirroring Drive's validator_set_update_v2 and Tenderdash's
/// proposer selection: the rest of the active set after the last proposer, then each other set in
/// rotation order (wrapping, never into the two oldest when there are more than ten), each in ProTx
/// order. Banned members are skipped. Returns each evonode's first turn: (ProTx hash, blocks from now,
/// 1 = the next block).
fn schedule(q: &Quorums) -> Vec<([u8; 32], u64)> {
    let sets: HashMap<&[u8], &ValidatorSet> = q.sets.iter().map(|s| (s.quorum_hash.as_slice(), s)).collect();
    // Each set once: the walk below is (sets in the order) × (their members).
    let mut listed = HashSet::new();
    let order: Vec<&[u8]> =
        q.order.iter().map(Vec::as_slice).filter(|h| sets.contains_key(h) && listed.insert(*h)).collect();
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
    // Drive moves on from a set among the two oldest to the first, so then every rotatable set is next.
    let others = if current < rotatable { rotatable - 1 } else { rotatable };
    let mut index = current;
    for _ in 0..others {
        index = if index + 1 >= rotatable { 0 } else { index + 1 };
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
fn turns_between(set: &ValidatorSet, from: &[u8; 32], to: &[u8; 32], blocks: u64) -> Option<Vec<([u8; 32], bool)>> {
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

/// Evonodes to ask, in random order and one per IP: those that passed platform.rs's latest check, or
/// every enabled evonode with an address if too few have.
struct Peers {
    list: Vec<SocketAddr>,
    next: usize,
    client: reqwest::Client,
    /// How many of them (by IP) passed platform.rs's latest check.
    healthy: usize,
    /// Answers from a chain state before this (unix ms) are passed over: evonodes left behind, like
    /// those that didn't upgrade and halted, still answer and agree with each other.
    min_time_ms: u64,
}

/// The public addresses among `addrs`, the first of each IP only: one host's evonodes count as one.
fn distinct_ips(addrs: &[String]) -> Vec<SocketAddr> {
    let mut seen = HashSet::new();
    addrs.iter().filter_map(|a| platform::target(a).ok()).filter(|a| seen.insert(a.ip())).collect()
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
        let healthy = distinct_ips(&healthy);
        let n_healthy = healthy.len();
        let mut list = if n_healthy >= 4 { healthy } else { distinct_ips(&all) };
        list.shuffle(&mut rand::rng());
        if list.len() < 2 {
            bail!("fewer than two evonodes (by IP) to ask");
        }
        Ok(Peers { list, next: 0, client: client.clone(), healthy: n_healthy, min_time_ms: 0 })
    }

    fn take(&mut self) -> SocketAddr {
        let a = self.list[self.next % self.list.len()];
        self.next += 1;
        a
    }

    /// One evonode's answer, unchecked: for what's only an estimate (the proposer schedule).
    async fn one<T>(
        &mut self,
        method: &str,
        request: &[u8],
        max: usize,
        parse: impl Fn(&[u8]) -> Result<(T, Meta)>,
    ) -> Result<(T, Meta)> {
        let mut error = None;
        for _ in 0..3.min(self.list.len()) {
            let addr = self.take();
            match platform::unary(&self.client, addr, method, request, max).await.and_then(|(m, _)| parse(&m)) {
                Ok(answer) => return Ok(answer),
                Err(e) => error = Some(format!("{addr}: {e:#}")),
            }
        }
        bail!("{method}: {}", error.unwrap_or_else(|| "no answer".into()))
    }

    /// Asks evonodes in turn until two at different IPs give the same answer at the same chain state
    /// (`agreed_meta`), and returns it with the metadata they agree on.
    async fn agreed<T: PartialEq>(
        &mut self,
        method: &str,
        request: &[u8],
        max: usize,
        parse: impl Fn(&[u8]) -> Result<(T, Meta)>,
    ) -> Result<(T, Meta)> {
        self.agreed_by(2, method, request, max, parse).await
    }

    /// As `agreed`, but `need` evonodes at different IPs have to give the answer.
    async fn agreed_by<T: PartialEq>(
        &mut self,
        need: usize,
        method: &str,
        request: &[u8],
        max: usize,
        parse: impl Fn(&[u8]) -> Result<(T, Meta)>,
    ) -> Result<(T, Meta)> {
        // Each answer given so far, with the chain state its evonodes agree on and their IPs.
        let mut answers: Vec<(T, Meta, Vec<IpAddr>)> = Vec::new();
        let mut error = None;
        for _ in 0..(MAX_ASKED + need - 2).min(self.list.len() + 1) {
            let addr = self.take();
            let answer = platform::unary(&self.client, addr, method, request, max).await.and_then(|(m, _)| parse(&m));
            match answer {
                Ok((_, m)) if m.time_ms < self.min_time_ms => {
                    error = Some(format!("{addr}: behind (chain time {})", m.time_ms / 1000));
                }
                Ok((t, m)) => {
                    let same = answers.iter_mut().find_map(|(at, am, ips)| {
                        let meta = agreed_meta(am, &m).filter(|_| !ips.contains(&addr.ip()) && *at == t)?;
                        Some((am, ips, meta))
                    });
                    match same {
                        Some((am, ips, meta)) => {
                            *am = meta;
                            ips.push(addr.ip());
                            if ips.len() >= need {
                                return Ok((t, meta));
                            }
                        }
                        None => answers.push((t, m, vec![addr.ip()])),
                    }
                }
                Err(e) => error = Some(format!("{addr}: {e:#}")),
            }
        }
        match (answers.is_empty(), error) {
            (false, _) => bail!("{method}: fewer than {need} evonodes agreed"),
            (true, e) => bail!("{method}: {}", e.unwrap_or_else(|| "no answer".into())),
        }
    }
}

// ---- the collector -------------------------------------------------------------------------------

/// The latest validator-set sample: who proposes when.
#[derive(Clone, Debug, Default)]
pub struct Schedule {
    ts: i64,
    height: u64,
    epoch: u64,
    /// When we took it (unix time): a schedule older than SCHEDULE_TTL isn't shown.
    fetched: i64,
    /// Seconds per block the times below assume.
    block_secs: f64,
    /// Blocks until each evonode's next turn (ProTx as displayed → 1 for the next block).
    slots: HashMap<String, u64>,
}

impl Schedule {
    /// When `protx` is expected to propose next (unix time), if it's in a rotating validator set.
    fn eta(&self, protx: &str) -> Option<i64> {
        self.slots.get(protx).map(|&s| self.ts + (s as f64 * self.block_secs).round() as i64)
    }
}

/// How long a validator-set sample stands for "who proposes next" when newer ones fail.
const SCHEDULE_TTL: i64 = 5 * 60;

/// The proposer schedule, unless it's too old to say anything (then every ETA is unknown).
pub fn fresh_schedule(ctx: &NetCtx) -> Option<Arc<Schedule>> {
    ctx.platform_schedule.read().unwrap().clone().filter(|s| db::now() - s.fetched <= SCHEDULE_TTL)
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

/// Our dashd's chain tip, once it's synced: no Core height an answer claims can be past it.
fn local_tip(ctx: &NetCtx) -> Option<u64> {
    let s = ctx.status.read().unwrap();
    (s.synced && s.blocks > 0).then_some(s.blocks as u64)
}

/// The chain-locked Core height two evonodes agree on, held to our dashd's tip.
fn held_core_height(agreed: u64, tip: Option<u64>) -> u64 {
    tip.map_or(agreed, |t| agreed.min(t))
}

/// Seconds per epoch (Drive's EPOCH_TIME_LENGTH_S: 788,400 in .env.mainnet, 3,600 in .env.testnet).
fn epoch_secs(net: Network) -> u64 {
    if net == Network::Mainnet { 788_400 } else { 3_600 }
}

/// Drive's epoch indexes are u16 (EpochInfoV0::calculate).
const MAX_EPOCH: u64 = u16::MAX as u64;
/// Evonodes (by IP) that have to agree on a page of finished epochs: it's stored once and never asked
/// for again.
const FINALIZED_AGREE: usize = 3;
/// Rounds in a row that fail to move on from the same stored epoch before the newest stored epochs
/// are fetched again (`recover_tail`).
const STALL_ROUNDS: u32 = 3;

/// What the collector carries from one round to the next.
#[derive(Debug, Default)]
struct Progress {
    /// Rounds in a row that found a new Platform chain (see `restart`).
    behind: u32,
    /// The newest stored epoch when finished epochs last failed to move on, and how many rounds in a
    /// row they have.
    stalled: (Option<u64>, u32),
}

impl Progress {
    fn stalled_at(&mut self, newest: Option<u64>) {
        self.stalled = (newest, if self.stalled.0 == newest { self.stalled.1 + 1 } else { 1 });
    }
}

pub async fn run(ctx: Arc<NetCtx>) {
    let client = platform::client();
    let explorer = explorer_client();
    // What's stored is served from the start, rather than gathered per request until a round ends.
    refresh_board(&ctx).await;
    wait_for_evonodes(&ctx).await;
    let mut progress = Progress::default();
    loop {
        if let Err(e) = round(&ctx, &client, &explorer, &mut progress).await {
            tracing::warn!(net = ctx.net.name(), "platform rewards: {e:#}");
        }
        refresh_board(&ctx).await;
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// A current epoch this many epochs below the newest stored one means Platform started over (testnet
/// resets), once RESET_ROUNDS rounds in a row see it; never on mainnet, whose Platform doesn't reset.
const RESET_MARGIN: u64 = 2;
const RESET_ROUNDS: u32 = 3;

/// Every Platform table the collector fills, for a reset.
const PLATFORM_TABLES: [&str; 9] = [
    "platform_epochs",
    "platform_payouts",
    "platform_epoch_blocks",
    "platform_balances",
    "platform_balance_history",
    "platform_votes",
    "platform_turns",
    "platform_turn_samples",
    "platform_explorer",
];

/// Clears what was collected from a Platform chain that's gone.
fn clear_platform(c: &mut Connection) -> Result<()> {
    let tx = c.transaction()?;
    for t in PLATFORM_TABLES {
        tx.execute(&format!("DELETE FROM {t}"), [])?;
    }
    tx.execute("DELETE FROM meta WHERE key = ?1", [NOW_KEY])?;
    tx.commit()?;
    Ok(())
}

/// What a current epoch far below the newest stored one means.
#[derive(Debug, PartialEq)]
enum Restart {
    /// It isn't.
    Go,
    /// Evonodes left behind (they started that epoch before the stored one), or a wrong stored epoch:
    /// wait, and look at the stored ones again (`recover_tail`).
    Lag,
    /// A new chain, seen fewer than RESET_ROUNDS rounds in a row.
    Pending,
    /// A new chain: clear what was collected.
    Clear,
}

/// A current epoch RESET_MARGIN or more below the newest stored one is a new chain only on testnet,
/// and only if it started after that stored epoch did (a chain that halted started its epoch earlier).
/// `behind` counts the rounds in a row that saw a new chain.
fn restart(net: Network, current: &Epoch, newest: Option<&Before>, behind: &mut u32) -> Restart {
    let Some(n) = newest.filter(|n| current.epoch.saturating_add(RESET_MARGIN) <= n.epoch) else {
        *behind = 0;
        return Restart::Go;
    };
    if net == Network::Mainnet || current.start_ms / 1000 <= n.first_block_time {
        *behind = 0;
        return Restart::Lag;
    }
    *behind += 1;
    if *behind < RESET_ROUNDS {
        return Restart::Pending;
    }
    *behind = 0;
    Restart::Clear
}

/// The current epoch, on its own: numbered as the answer's metadata says, within Drive's u16 epoch
/// indexes, started in the past, and at or below the chain-locked height the answer reports.
fn check_current(e: &Epoch, meta: &Meta, now: u64) -> Result<()> {
    if e.epoch != meta.epoch {
        bail!("epoch {} answered at epoch {}", e.epoch, meta.epoch);
    }
    if e.epoch > MAX_EPOCH {
        bail!("epoch {} is past Drive's last epoch index", e.epoch);
    }
    if e.start_ms / 1000 > now + SKEW_SECS {
        bail!("epoch {} starts in the future", e.epoch);
    }
    if e.first_core_height > meta.core_height {
        bail!("epoch {} starts at Core block {}, past the answer's {}", e.epoch, e.first_core_height, meta.core_height);
    }
    Ok(())
}

/// Drive numbers epochs by time: a block at `t` is in epoch ⌊(t − genesis) / d⌋ (EpochInfoV0::calculate),
/// so epoch n's first block is in [genesis + n·d, genesis + (n + 1)·d). Two epochs' starts are then their
/// distance in epochs times d apart, give or take less than d (d itself in whole seconds). `a` and `b`
/// are (epoch, start), `a` the earlier.
fn starts_fit(a: (u64, u64), b: (u64, u64), d: u64) -> bool {
    match (b.0.checked_sub(a.0), b.1.checked_sub(a.1)) {
        (Some(k), Some(dt)) => k.checked_mul(d).is_some_and(|x| x.abs_diff(dt) <= d),
        _ => false,
    }
}

/// The current epoch against the stored ones: after the newest (in epochs, blocks and Core blocks),
/// and numbered as its start time says from the newest and from the oldest.
fn check_current_follows(e: &Epoch, newest: Option<&Before>, oldest: Option<&Before>, d: u64) -> Result<()> {
    let start = e.start_ms / 1000;
    for b in newest.into_iter().chain(oldest) {
        if e.epoch <= b.epoch || !starts_fit((b.epoch, b.first_block_time), (e.epoch, start), d) {
            bail!(
                "epoch {} (started {start}) doesn't follow stored epoch {} (started {})",
                e.epoch,
                b.epoch,
                b.first_block_time
            );
        }
    }
    if let Some(b) = newest
        && (e.first_block_height < b.end_block || e.first_core_height < b.next_core_height)
    {
        bail!("epoch {} starts before stored epoch {} ended", e.epoch, b.epoch);
    }
    Ok(())
}

/// The last finished epoch before the current one ends where the current one starts: at its first
/// Platform block, and at its Core start (getEpochsInfo gives the current epoch's Core start as is; a
/// finished epoch's first Core block is the one after it).
fn check_ties(last: &Before, current: &Epoch, d: u64) -> Result<()> {
    if !starts_fit((last.epoch, last.first_block_time), (current.epoch, current.start_ms / 1000), d)
        || last.end_block != current.first_block_height
        || last.next_core_height != current.first_core_height
    {
        bail!(
            "epoch {} ends at block {} and Core block {}, but epoch {} starts at {} and {}",
            last.epoch,
            last.end_block,
            last.next_core_height,
            current.epoch,
            current.first_block_height,
            current.first_core_height
        );
    }
    Ok(())
}

async fn round(
    ctx: &NetCtx,
    client: &reqwest::Client,
    explorer: &reqwest::Client,
    progress: &mut Progress,
) -> Result<()> {
    let mut peers = Peers::load(ctx, client).await?;
    let tip = local_tip(ctx);
    let now_secs = db::now() as u64;
    let net = ctx.net.name();
    let limits = Limits { tip, now: now_secs, epoch_secs: epoch_secs(ctx.net) };
    if progress.stalled.1 >= STALL_ROUNDS {
        progress.stalled.1 = 0;
        match recover_tail(ctx, &mut peers, limits).await {
            Ok(Some(from)) => tracing::warn!(
                net,
                "platform rewards: stored epochs from {from} on aren't what {FINALIZED_AGREE} evonodes give now; dropped them to fetch again"
            ),
            Ok(None) => tracing::info!(net, "platform rewards: the newest stored epochs are what evonodes give now"),
            Err(e) => tracing::warn!(net, "platform rewards: checking the newest stored epochs: {e:#}"),
        }
    }
    let (newest, oldest) = ctx.db.read(stored_ends).await?;
    // Evonodes whose chain is older than the newest stored epoch are behind: ask others.
    peers.min_time_ms = newest.map_or(0, |b| b.first_block_time * 1000);
    // The current epoch: getEpochsInfo { count = 2: 1, ascending = 3: false }, from the latest epoch.
    let (epochs, meta) = peers.agreed("getEpochsInfo", &v0(&[0x10, 0x01]), SMALL_ANSWER, parse_epochs).await?;
    let current = *epochs.first().ok_or_else(|| anyhow!("getEpochsInfo: no epoch"))?;
    check_current(&current, &meta, now_secs)?;
    let stored_epoch = newest.map(|b| b.epoch);
    match restart(ctx.net, &current, newest.as_ref(), &mut progress.behind) {
        Restart::Go => {
            check_current_follows(&current, newest.as_ref(), oldest.as_ref(), limits.epoch_secs)
                .inspect_err(|_| progress.stalled_at(stored_epoch))?;
        }
        r @ (Restart::Lag | Restart::Pending) => {
            if r == Restart::Lag {
                progress.stalled_at(stored_epoch);
            }
            bail!(
                "Platform is at epoch {} (started {}), but epoch {} is stored; not collecting",
                current.epoch,
                current.start_ms / 1000,
                stored_epoch.unwrap_or_default()
            );
        }
        Restart::Clear => {
            tracing::warn!(
                net,
                "platform rewards: Platform restarted at epoch {} (epoch {} was stored); clearing what was collected",
                current.epoch,
                stored_epoch.unwrap_or_default()
            );
            ctx.db.write(clear_platform).await?;
            *ctx.platform_schedule.write().unwrap() = None;
            peers.min_time_ms = 0;
        }
    }
    // Chain heights and time as both evonodes put them, held to our tip and clock.
    let core_height = held_core_height(meta.core_height, tip);
    let time = (meta.time_ms / 1000).min(now_secs);
    // Each part stands alone: one that fails (live numbers can move between two evonodes' answers)
    // keeps its last values and is tried again next round.
    let stored = if let Some(t) = tip.filter(|t| *t < current.first_core_height) {
        // Finished epochs end at the current one's Core start, which our dashd hasn't reached.
        tracing::info!(net, "platform rewards: our dashd is at Core block {t}, behind Platform; finished epochs wait");
        Some(0)
    } else {
        let r = finalized(ctx, &mut peers, &current, limits).await;
        match &r {
            Ok(_) => progress.stalled = (None, 0),
            Err(_) => progress.stalled_at(stored_epoch),
        }
        soft(net, "finished epochs", r)
    };
    let blocks = soft(net, "blocks this epoch", epoch_blocks(&mut peers, current.epoch).await);
    let ids = ctx.db.read(registered_evonodes).await?;
    let balances = soft(net, "balances", balances(&mut peers, &ids).await);
    let versions = soft(
        net,
        "upgrade state",
        peers.agreed("getProtocolVersionUpgradeState", &v0(&[]), SMALL_ANSWER, parse_upgrade_state).await.map(|v| v.0),
    );
    let votes = soft(net, "votes", votes(&mut peers).await);
    // For the log: how many of each came back ("failed" for a part that didn't).
    let n = |x: Option<usize>| x.map_or_else(|| "failed".to_owned(), |n| n.to_string());
    let counts =
        (n(blocks.as_ref().map(Vec::len)), n(balances.as_ref().map(Vec::len)), n(votes.as_ref().map(BTreeMap::len)));
    let network = ctx.net;
    ctx.db
        .write(move |c| {
            let tx = c.transaction()?;
            let now_ts = db::now();
            // Without a fresh vote count, the last one stands.
            let versions = match versions {
                Some(v) => json!(v),
                None => self::current(&tx)?.map_or(json!([]), |n| n["versions"].clone()),
            };
            let now = json!({
                "epoch": current.epoch, "start": current.start_ms / 1000, "first_height": current.first_block_height,
                "first_core_height": current.first_core_height,
                "protocol_version": meta.protocol_version.max(current.protocol_version), "height": meta.height,
                "core_height": core_height, "time": time, "versions": versions, "updated": now_ts,
            });
            if let Some(blocks) = &blocks {
                tx.execute("DELETE FROM platform_epoch_blocks", [])?;
                let mut st =
                    tx.prepare("INSERT INTO platform_epoch_blocks (protx, epoch, blocks) VALUES (?1, ?2, ?3)")?;
                for (id, b) in blocks {
                    st.execute(params![hex(id), current.epoch as i64, *b as i64])?;
                }
            }
            if let Some(balances) = &balances {
                let mut latest = tx.prepare(
                    "INSERT INTO platform_balances (protx, credits, ts) VALUES (?1, ?2, ?3)
                     ON CONFLICT(protx) DO UPDATE SET credits = excluded.credits, ts = excluded.ts",
                )?;
                let mut history = tx.prepare(
                    "INSERT OR IGNORE INTO platform_balance_history (protx, ts, credits)
                     SELECT ?1, ?2, ?3 WHERE ?3 IS NOT (SELECT credits FROM platform_balance_history
                        WHERE protx = ?1 ORDER BY ts DESC LIMIT 1)",
                )?;
                for (id, credits) in balances {
                    let credits = credits.and_then(|v| i64::try_from(v).ok());
                    latest.execute(params![hex(id), credits, now_ts])?;
                    if let Some(v) = credits {
                        history.execute(params![hex(id), now_ts, v])?;
                    }
                }
            }
            if let Some(votes) = &votes {
                tx.execute("DELETE FROM platform_votes", [])?;
                let mut st = tx.prepare("INSERT INTO platform_votes (protx, version, ts) VALUES (?1, ?2, ?3)")?;
                for (id, v) in votes {
                    st.execute(params![hex(id), *v as i64, now_ts])?;
                }
            }
            set_meta(&tx, NOW_KEY, &now.to_string())?;
            prune(&tx, network, now_ts)?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    tracing::info!(
        net,
        "platform rewards: epoch {}, {} new finished epochs, {} proposers so far, {} balances, {} votes",
        current.epoch,
        n(stored),
        counts.0,
        counts.1,
        counts.2
    );
    if ctx.net == Network::Mainnet {
        explorer_totals(ctx, explorer).await;
    }
    Ok(())
}

/// How long proposer turns and balance history are kept: a testnet epoch is an hour, so its history
/// is kept for a month only.
fn retention(net: Network) -> i64 {
    if net == Network::Mainnet { 400 * DAY } else { 31 * DAY }
}

/// Drops turns of epochs that started before the retention window, and balance changes older than it
/// (each evonode's latest stays: new rows are only added when it changes).
fn prune(c: &Connection, net: Network, now: i64) -> Result<()> {
    let since = now - retention(net);
    let first: Option<i64> =
        c.query_row("SELECT min(epoch) FROM platform_epochs WHERE first_block_time >= ?1", [since], |r| r.get(0))?;
    if let Some(first) = first {
        c.execute("DELETE FROM platform_turns WHERE epoch < ?1", [first])?;
        c.execute("DELETE FROM platform_turn_samples WHERE epoch < ?1", [first])?;
    }
    c.execute(
        "DELETE FROM platform_balance_history WHERE ts < ?1
           AND ts < (SELECT max(h.ts) FROM platform_balance_history h WHERE h.protx = platform_balance_history.protx)",
        [since],
    )?;
    Ok(())
}

/// `r`'s value, or None with a warning.
fn soft<T>(net: &str, what: &str, r: Result<T>) -> Option<T> {
    r.map_err(|e| tracing::warn!(net, "platform rewards: {what}: {e:#}")).ok()
}

/// What a finished epoch is checked against: a stored one before it (the newest, or the oldest for
/// its number), or the one before it in the same page.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Before {
    epoch: u64,
    first_block_time: u64,
    /// Where the next epoch starts: its first Platform block plus its blocks.
    end_block: u64,
    next_core_height: u64,
}

impl Before {
    fn of(e: &Finalized) -> Before {
        Before {
            epoch: e.epoch,
            first_block_time: e.first_block_time_ms / 1000,
            end_block: e.first_block_height.saturating_add(e.total_blocks),
            next_core_height: e.next_core_height,
        }
    }
}

/// The newest and the oldest stored epochs.
fn stored_ends(c: &Connection) -> Result<(Option<Before>, Option<Before>)> {
    let end = |order: &str| {
        c.query_row(
            &format!(
                "SELECT epoch, first_block_time, first_block_height + total_blocks, next_core_height
                 FROM platform_epochs ORDER BY epoch {order} LIMIT 1"
            ),
            [],
            |r| {
                Ok(Before {
                    epoch: r.get::<_, i64>(0)? as u64,
                    first_block_time: r.get::<_, i64>(1)? as u64,
                    end_block: r.get::<_, i64>(2)? as u64,
                    next_core_height: r.get::<_, i64>(3)? as u64,
                })
            },
        )
        .optional()
    };
    Ok((end("DESC")?, end("ASC")?))
}

/// What finished epochs are held to besides each other: our dashd's tip, our clock, the epoch length.
#[derive(Clone, Copy, Debug)]
struct Limits {
    tip: Option<u64>,
    now: u64,
    epoch_secs: u64,
}

/// Checks a page of finished epochs asked for as `range` against Drive's rules, so evonodes agreeing
/// can't make up epochs or payouts:
/// - in range and in order, within Drive's u16 indexes, not in the future, and numbered as their start
///   says (`starts_fit`, from the one before and from `oldest`, the oldest stored);
/// - each starting where the one before ended: at the Platform block after its last, and at the Core
///   block after its `next_core_height`;
/// - its proposers' blocks adding up to its total, at least one;
/// - Core heights at or below our tip, and its Core block rewards exactly what Drive pays for them;
/// - with `current` (the page ends just before it), the last ending where `current` starts.
fn check_finalized(
    epochs: &[Finalized],
    range: RangeInclusive<u64>,
    mut before: Option<Before>,
    mut oldest: Option<Before>,
    current: Option<&Epoch>,
    limits: Limits,
) -> Result<()> {
    let mut last_epoch = None;
    for e in epochs {
        let n = e.epoch;
        if !range.contains(&n) || last_epoch.is_some_and(|l| n <= l) {
            bail!("epoch {n} wasn't asked for, or is out of order ({}–{})", range.start(), range.end());
        }
        last_epoch = Some(n);
        if n > MAX_EPOCH {
            bail!("epoch {n} is past Drive's last epoch index");
        }
        let mut ids = HashSet::new();
        let mut blocks = 0u64;
        for (id, b) in &e.proposers {
            if !ids.insert(id) {
                bail!("epoch {n}: a proposer is listed twice");
            }
            blocks = blocks.checked_add(*b).ok_or_else(|| anyhow!("epoch {n}: blocks overflow"))?;
        }
        if blocks != e.total_blocks {
            bail!("epoch {n}: its proposers made {blocks} blocks, not {}", e.total_blocks);
        }
        if blocks == 0 {
            bail!("epoch {n} has no blocks");
        }
        let pool = e.processing_fees.checked_add(e.storage_fees).and_then(|x| x.checked_add(e.core_rewards));
        if pool.is_none_or(|p| p > i64::MAX as u64) || e.total_blocks > i64::MAX as u64 {
            bail!("epoch {n}: pool or blocks out of range");
        }
        // An epoch over which the chain-locked height didn't move starts just after it and ends at it.
        if e.first_core_height > e.next_core_height.saturating_add(1)
            || limits.tip.is_some_and(|t| e.next_core_height > t)
        {
            bail!("epoch {n}: Core blocks {}–{} aren't before our tip", e.first_core_height, e.next_core_height);
        }
        // Epoch 0 starts at Platform's genesis Core height, which isn't in its record.
        if n > 0 && core_rewards(e.first_core_height, e.next_core_height, CORE_HALVING) != Some(e.core_rewards) {
            bail!("epoch {n}: Core block rewards {} don't match its Core blocks", e.core_rewards);
        }
        let this = Before::of(e);
        let time = this.first_block_time;
        if time > limits.now + SKEW_SECS {
            bail!("epoch {n} starts in the future");
        }
        if let Some(b) = before {
            if time <= b.first_block_time {
                bail!("epoch {n} starts no later than the one before it");
            }
            if b.end_block != e.first_block_height {
                bail!(
                    "epoch {n} starts at block {}, not after the one before it ({})",
                    e.first_block_height,
                    b.end_block
                );
            }
            if b.next_core_height.checked_add(1) != Some(e.first_core_height) {
                bail!(
                    "epoch {n} starts at Core block {}, not after the one before it ({})",
                    e.first_core_height,
                    b.next_core_height
                );
            }
        }
        let first = *oldest.get_or_insert(this);
        for b in before.iter().chain([&first]) {
            if !starts_fit((b.epoch, b.first_block_time), (n, time), limits.epoch_secs) {
                bail!(
                    "epoch {n} starts at {time}, out of step with epoch {} (started {})",
                    b.epoch,
                    b.first_block_time
                );
            }
        }
        before = Some(this);
    }
    if let (Some(c), Some(b)) = (current, before) {
        check_ties(&b, c, limits.epoch_secs)?;
    }
    Ok(())
}

/// getFinalizedEpochInfos { start_epoch_index = 1, start_included = 2, end_epoch_index = 3, end_included = 4 }.
fn finalized_request(start: u64, end: u64) -> Vec<u8> {
    let mut req = Vec::new();
    put_uint(&mut req, 1, start);
    put_uint(&mut req, 2, 1);
    put_uint(&mut req, 3, end);
    put_uint(&mut req, 4, 1);
    v0(&req)
}

/// Fetches and stores finished epochs after the newest stored one, up to the one before `current`,
/// and checks the last of them ends where `current` starts. Returns how many were stored.
async fn finalized(ctx: &NetCtx, peers: &mut Peers, current: &Epoch, limits: Limits) -> Result<usize> {
    let (mut before, mut oldest) = ctx.db.read(stored_ends).await?;
    let mut start = before.map_or(0, |b| b.epoch + 1);
    let mut stored = 0;
    for _ in 0..FINALIZED_PAGES {
        if start >= current.epoch {
            // Caught up: the newest stored epoch is the last before the current one.
            if let Some(b) = &before {
                check_ties(b, current, limits.epoch_secs)?;
            }
            break;
        }
        if peers.healthy < FINALIZED_AGREE {
            tracing::info!(
                net = ctx.net.name(),
                "platform rewards: finished epochs wait for {FINALIZED_AGREE} healthy evonodes (by IP) to ask"
            );
            break;
        }
        let end = (start + PAGE as u64 - 1).min(current.epoch - 1);
        let request = finalized_request(start, end);
        let (epochs, _) = peers
            .agreed_by(FINALIZED_AGREE, "getFinalizedEpochInfos", &request, FINALIZED_ANSWER, parse_finalized)
            .await?;
        let next = (end + 1 == current.epoch).then_some(current);
        check_finalized(&epochs, start..=end, before, oldest, next, limits)?;
        stored += epochs.len();
        if let Some(e) = epochs.last() {
            before = Some(Before::of(e));
            oldest = oldest.or_else(|| epochs.first().map(Before::of));
            ctx.db.write(move |c| store_finalized(c, &epochs)).await?;
        }
        start = end + 1;
    }
    Ok(stored)
}

/// The newest `n` stored epochs, oldest first, with their proposers as stored (from their payouts).
fn newest_stored(c: &Connection, n: usize) -> Result<Vec<Finalized>> {
    let mut epochs: Vec<Finalized> = query_all(
        c,
        "SELECT epoch, first_block_height, first_core_height, first_block_time, protocol_version, total_blocks,
                next_core_height, processing_fees, storage_fees, core_rewards
         FROM platform_epochs ORDER BY epoch DESC LIMIT ?1",
        [n as i64],
        |r| {
            let u = |i| r.get::<_, i64>(i).map(|v| v as u64);
            Ok(Finalized {
                epoch: u(0)?,
                first_block_height: u(1)?,
                first_core_height: u(2)?,
                first_block_time_ms: u(3)? * 1000,
                protocol_version: u(4)?,
                total_blocks: u(5)?,
                next_core_height: u(6)?,
                processing_fees: u(7)?,
                storage_fees: u(8)?,
                core_rewards: u(9)?,
                proposers: Vec::new(),
            })
        },
    )?;
    epochs.reverse();
    let mut proposers: HashMap<u64, Vec<([u8; 32], u64)>> = HashMap::new();
    let from = epochs.first().map_or(0, |e| e.epoch as i64);
    let rows: Vec<(i64, String, i64)> =
        query_all(c, "SELECT epoch, protx, blocks FROM platform_payouts WHERE epoch >= ?1", [from], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
    for (epoch, protx, blocks) in rows {
        let id = unhex(&protx).ok_or_else(|| anyhow!("stored proposer {protx}"))?;
        proposers.entry(epoch as u64).or_default().push((id, blocks as u64));
    }
    for e in &mut epochs {
        e.proposers = proposers.remove(&e.epoch).unwrap_or_default();
        e.proposers.sort_unstable();
    }
    Ok(epochs)
}

/// The first stored epoch that `fresh`, an agreed answer for the same epochs at chain state `meta`,
/// shows to be wrong: one it gives differently; one it has that wasn't stored, with stored ones after
/// it; or one it doesn't have although its chain is past it, in epochs or (as epochs are numbered by
/// time) in time. Its chain may be behind ours, so an epoch it hasn't reached proves nothing.
fn first_wrong(stored: &[Finalized], fresh: &[Finalized], meta: &Meta) -> Option<u64> {
    let fresh: BTreeMap<u64, Finalized> = fresh
        .iter()
        .map(|e| {
            let mut e = e.clone();
            e.first_block_time_ms = e.first_block_time_ms / 1000 * 1000;
            e.proposers.sort_unstable();
            (e.epoch, e)
        })
        .collect();
    let kept: HashSet<u64> = stored.iter().map(|e| e.epoch).collect();
    let newest = stored.last().map_or(0, |e| e.epoch);
    let skipped = fresh.keys().copied().find(|n| !kept.contains(n) && *n < newest);
    let wrong = stored
        .iter()
        .find(|s| match fresh.get(&s.epoch) {
            Some(f) => f != *s,
            None => {
                s.epoch < meta.epoch || (s.epoch > meta.epoch && s.first_block_time_ms / 1000 < meta.time_ms / 1000)
            }
        })
        .map(|s| s.epoch);
    skipped.into_iter().chain(wrong).min()
}

/// Drops the stored epochs from `from` on, with their payouts.
fn drop_epochs_from(c: &mut Connection, from: u64) -> Result<()> {
    let tx = c.transaction()?;
    tx.execute("DELETE FROM platform_payouts WHERE epoch >= ?1", [from as i64])?;
    tx.execute("DELETE FROM platform_epochs WHERE epoch >= ?1", [from as i64])?;
    tx.commit()?;
    Ok(())
}

/// After STALL_ROUNDS rounds failed to move on from the stored epochs: the newest of them (a page)
/// again, from three evonodes at different IPs. Evonodes agreeing on a wrong epoch would otherwise
/// stop collection for good, as no true epoch follows it. From the first one the answer shows to be
/// wrong (`first_wrong`), stored epochs and their payouts are dropped, to be fetched again. Returns
/// that epoch.
async fn recover_tail(ctx: &NetCtx, peers: &mut Peers, limits: Limits) -> Result<Option<u64>> {
    if peers.healthy < FINALIZED_AGREE {
        bail!("fewer than {FINALIZED_AGREE} healthy evonodes (by IP) to ask");
    }
    let stored = ctx.db.read(|c| newest_stored(c, PAGE)).await?;
    let (Some(lo), Some(hi)) = (stored.first().map(|e| e.epoch), stored.last().map(|e| e.epoch)) else {
        return Ok(None);
    };
    let request = finalized_request(lo, hi);
    let (fresh, meta) =
        peers.agreed_by(FINALIZED_AGREE, "getFinalizedEpochInfos", &request, FINALIZED_ANSWER, parse_finalized).await?;
    check_finalized(&fresh, lo..=hi, None, None, None, limits)?;
    let Some(from) = first_wrong(&stored, &fresh, &meta) else { return Ok(None) };
    ctx.db.write(move |c| drop_epochs_from(c, from)).await?;
    Ok(Some(from))
}

fn store_finalized(c: &mut Connection, epochs: &[Finalized]) -> Result<()> {
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
/// limit = 2, start_after = 3 }, a page at a time, each in ProTx order after the one before.
async fn epoch_blocks(peers: &mut Peers, epoch: u64) -> Result<Vec<([u8; 32], u64)>> {
    let mut out: Vec<([u8; 32], u64)> = Vec::new();
    for _ in 0..MAX_PAGES {
        let mut req = Vec::new();
        put_uint(&mut req, 1, epoch);
        put_uint(&mut req, 2, PAGE as u64);
        if let Some((last, _)) = out.last() {
            put_bytes(&mut req, 3, last);
        }
        let (page, _) =
            peers.agreed("getEvonodesProposedEpochBlocksByRange", &v0(&req), SMALL_ANSWER, parse_pairs).await?;
        // Each page has to move on, or the next would ask for the same one again.
        let mut after = out.last().map(|p| p.0);
        for (id, _) in &page {
            if after.is_some_and(|a| *id <= a) {
                bail!("getEvonodesProposedEpochBlocksByRange: a page out of order");
            }
            after = Some(*id);
        }
        let n = page.len();
        out.extend(page.into_iter().map(|(id, b)| (id, b.unwrap_or(0))));
        if n < PAGE {
            return Ok(out);
        }
    }
    bail!("getEvonodesProposedEpochBlocksByRange: more than {MAX_PAGES} pages")
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
        out.extend(peers.agreed("getIdentitiesBalances", &v0(&req), SMALL_ANSWER, parse_pairs).await?.0);
    }
    Ok(out)
}

/// Every evonode's protocol version vote: getProtocolVersionUpgradeVoteStatus { start_pro_tx_hash = 1,
/// count = 2 }, a page at a time from the first ProTx (each page starts at, and repeats, the last one).
async fn votes(peers: &mut Peers) -> Result<BTreeMap<[u8; 32], u64>> {
    let mut out = BTreeMap::new();
    let mut start: Option<[u8; 32]> = None;
    for _ in 0..MAX_PAGES {
        let mut req = Vec::new();
        if let Some(s) = &start {
            put_bytes(&mut req, 1, s);
        }
        put_uint(&mut req, 2, PAGE as u64);
        let (page, _) =
            peers.agreed("getProtocolVersionUpgradeVoteStatus", &v0(&req), SMALL_ANSWER, parse_pairs).await?;
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
    bail!("getProtocolVersionUpgradeVoteStatus: more than {MAX_PAGES} pages")
}

// ---- validator sets ------------------------------------------------------------------------------

/// Validator sets are sampled every QUORUM_INTERVAL from one evonode, for the proposer schedule (an
/// estimate), and every AGREE_EVERY-th time from two that agree, for the turns and memberships stored:
/// the answer is ~126 KB, so this keeps a day's traffic near 150 MB instead of 240.
const AGREE_EVERY: u32 = 4;
/// How often failed samples are logged as warnings (the rest go to debug).
const SAMPLE_WARN_EVERY: Duration = Duration::from_secs(15 * 60);

pub async fn run_quorums(ctx: Arc<NetCtx>) {
    let client = platform::client();
    wait_for_evonodes(&ctx).await;
    let mut prev: Option<(Quorums, Meta)> = None;
    let mut since_agreed = AGREE_EVERY;
    let mut warned: Option<Instant> = None;
    loop {
        match sample(&ctx, &client, since_agreed >= AGREE_EVERY, prev.as_ref()).await {
            Ok(Some(s)) => {
                prev = Some(s);
                since_agreed = 1;
            }
            Ok(None) => since_agreed += 1,
            Err(e) if warned.is_none_or(|w| w.elapsed() >= SAMPLE_WARN_EVERY) => {
                warned = Some(Instant::now());
                tracing::warn!(net = ctx.net.name(), "validator sets: {e:#}");
            }
            Err(e) => tracing::debug!(net = ctx.net.name(), "validator sets: {e:#}"),
        }
        tokio::time::sleep(QUORUM_INTERVAL).await;
    }
}

/// Seconds between the samples turns are counted over.
fn agreed_interval() -> f64 {
    QUORUM_INTERVAL.as_secs_f64() * f64::from(AGREE_EVERY)
}

/// The turns between two agreed samples of the same validator set and epoch, when they can be pinned:
/// the newer at most as many blocks later as the set has members, and later in time.
fn turns_since(prev: &(Quorums, Meta), q: &Quorums, meta: &Meta) -> Vec<([u8; 32], bool)> {
    let (pq, pm) = prev;
    let Some(set) = q.sets.iter().find(|s| s.quorum_hash == q.current) else { return Vec::new() };
    let blocks = meta.height.saturating_sub(pm.height);
    if pq.current != q.current
        || pm.epoch != meta.epoch
        || meta.time_ms <= pm.time_ms
        || blocks > set.members.len() as u64
    {
        return Vec::new();
    }
    turns_between(set, &pq.last_proposer, &q.last_proposer, blocks).unwrap_or_default()
}

/// Samples the validator sets and updates the proposer schedule. With `agree`, two evonodes have to
/// agree, and the turns since `prev` (the last agreed sample) and the memberships are stored; the
/// sample is returned. Otherwise one evonode's answer only updates the schedule.
async fn sample(
    ctx: &NetCtx,
    client: &reqwest::Client,
    agree: bool,
    prev: Option<&(Quorums, Meta)>,
) -> Result<Option<(Quorums, Meta)>> {
    let mut peers = Peers::load(ctx, client).await?;
    let request = v0(&[]);
    let (q, mut meta) = if agree {
        peers.agreed("getCurrentQuorumsInfo", &request, QUORUMS_ANSWER, parse_quorums).await?
    } else {
        peers.one("getCurrentQuorumsInfo", &request, QUORUMS_ANSWER, parse_quorums).await?
    };
    let now = db::now();
    meta.time_ms = meta.time_ms.min(now as u64 * 1000);
    let net = ctx.net;
    let block_secs = ctx.db.read(move |c| block_secs(c, net, meta.height, meta.time_ms / 1000)).await?;
    let slots = schedule(&q).into_iter().map(|(raw, slot)| (hex(&raw), slot)).collect();
    *ctx.platform_schedule.write().unwrap() = Some(Arc::new(Schedule {
        ts: (meta.time_ms / 1000) as i64,
        height: meta.height,
        epoch: meta.epoch,
        fetched: now,
        block_secs,
        slots,
    }));
    if !agree {
        return Ok(None);
    }
    let turns = prev.map(|p| turns_since(p, &q, &meta)).unwrap_or_default();
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
    Ok(Some((q, meta)))
}

/// The newest finalized epoch that a later one followed: its blocks and its length in seconds.
fn latest_span(c: &Connection) -> rusqlite::Result<Option<(i64, i64)>> {
    c.query_row(
        "SELECT e.total_blocks, n.first_block_time - e.first_block_time FROM platform_epochs e
         JOIN platform_epochs n ON n.epoch = e.epoch + 1 ORDER BY e.epoch DESC LIMIT 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
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
    Ok(match latest_span(c)? {
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
const EXPLORER_MAX: usize = 256 << 10;

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
    let mut r = client.get(format!("{EXPLORER}/validator/{}", protx.to_ascii_uppercase())).send().await?;
    if r.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok((None, None));
    }
    if !r.status().is_success() {
        bail!("HTTP {}", r.status());
    }
    // Read up to a cap: a real answer is a few KB.
    let mut body = Vec::new();
    while let Some(chunk) = r.chunk().await? {
        if body.len() + chunk.len() > EXPLORER_MAX {
            bail!("answer too large");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(parse_explorer(&serde_json::from_slice(&body)?))
}

fn parse_explorer(v: &Value) -> (Option<u64>, Option<u64>) {
    // Numbers, or numeric strings for the big ones.
    let num = |x: &Value| x.as_u64().or_else(|| x.as_str().and_then(|s| s.parse().ok()));
    (num(&v["totalReward"]), num(&v["epochInfo"]["number"]))
}

// ---- what the API serves -------------------------------------------------------------------------

/// The ProTx hashes of every registered evonode.
fn registered_evonodes(c: &Connection) -> Result<Vec<String>> {
    Ok(query_all(c, "SELECT protx FROM masternodes WHERE type = 'Evo' AND removed IS NULL", [], |r| r.get(0))?)
}

/// The current epoch as the collector last read it (see `round`).
fn current(c: &Connection) -> Result<Option<Value>> {
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
        None => latest_finalized(c)?,
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
fn platform_yield(c: &Connection, protx: &str, since: i64) -> Result<Option<f64>> {
    let (y, span) = yields(c, since, Some(protx))?;
    Ok((span > 0.0).then(|| y.get(protx).copied().unwrap_or(0.0)))
}

/// When the current epoch started (unix seconds); now, before the first read.
fn epoch_start(now: Option<&Value>) -> i64 {
    now.and_then(|n| n["start"].as_i64()).unwrap_or_else(db::now)
}

fn current_start(c: &Connection) -> Result<i64> {
    Ok(epoch_start(current(c)?.as_ref()))
}

/// The newest finalized epoch.
fn latest_finalized(c: &Connection) -> Result<Option<i64>> {
    Ok(c.query_row("SELECT max(epoch) FROM platform_epochs", [], |r| r.get(0))?)
}

/// Blocks each evonode has proposed so far in `epoch`.
fn blocks_in(c: &Connection, epoch: Option<i64>) -> Result<HashMap<String, i64>> {
    Ok(query_all(c, "SELECT protx, blocks FROM platform_epoch_blocks WHERE epoch = ?1", [epoch], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })?)
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
    core_rewards(first.checked_add(1)?, to, CORE_HALVING)
}

/// Everything per evonode the network table, the list and evonode pages need, keyed by ProTx. Built
/// once per round (`refresh_board`), not per request: the network-wide sums read every payout of a month.
pub struct Board {
    now: Option<Value>,
    last: HashMap<String, (i64, i64)>,
    d30: HashMap<String, i64>,
    /// Every enabled evonode's 30-day rewards (zeros count), ascending: where one sits among them.
    d30_enabled: Vec<i64>,
    monthly: HashMap<String, i64>,
    /// The span `monthly` is at the pace of, in days.
    monthly_basis: Option<f64>,
    epoch_blocks: HashMap<String, i64>,
    epoch_total: i64,
    balances: HashMap<String, Option<i64>>,
    votes: HashMap<String, i64>,
}

fn board(c: &Connection) -> Result<Board> {
    let now = current(c)?;
    let start = epoch_start(now.as_ref());
    let last = query_all(
        c,
        "SELECT protx, blocks, credits FROM platform_payouts WHERE epoch = ?1",
        [latest_finalized(c)?],
        |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))),
    )?;
    let epoch_blocks = blocks_in(c, now.as_ref().and_then(|n| n["epoch"].as_i64()))?;
    let d30 = earned_since(c, start, db::now() - 30 * DAY)?;
    let enabled: Vec<String> = query_all(
        c,
        "SELECT protx FROM masternodes WHERE type = 'Evo' AND removed IS NULL AND coalesce(pose_ban_height, -1) <= 0",
        [],
        |r| r.get(0),
    )?;
    let mut d30_enabled: Vec<i64> = enabled.iter().map(|p| d30.get(p).copied().unwrap_or(0)).collect();
    d30_enabled.sort_unstable();
    let (monthly, monthly_basis) = monthly(c, start)?;
    Ok(Board {
        last,
        d30,
        d30_enabled,
        monthly,
        monthly_basis,
        epoch_total: epoch_blocks.values().sum(),
        epoch_blocks,
        balances: query_all(c, "SELECT protx, credits FROM platform_balances", [], |r| Ok((r.get(0)?, r.get(1)?)))?,
        votes: query_all(c, "SELECT protx, version FROM platform_votes", [], |r| Ok((r.get(0)?, r.get(1)?)))?,
        now,
    })
}

/// Rebuilds the board from what's stored now.
pub async fn refresh_board(ctx: &NetCtx) {
    match ctx.db.read(board).await {
        Ok(b) => *ctx.platform_board.write().unwrap() = Some(Arc::new(b)),
        Err(e) => tracing::warn!(net = ctx.net.name(), "platform rewards: board: {e:#}"),
    }
}

/// What the API's Platform answers start from: the proposer schedule while it's fresh, and the board.
pub struct View {
    schedule: Option<Arc<Schedule>>,
    board: Option<Arc<Board>>,
}

pub fn view(ctx: &NetCtx) -> View {
    View { schedule: fresh_schedule(ctx), board: ctx.platform_board.read().unwrap().clone() }
}

impl View {
    fn schedule(&self) -> Option<&Schedule> {
        self.schedule.as_deref()
    }

    /// The board, built here if no round has built it yet.
    fn board(&self, c: &Connection) -> Result<Arc<Board>> {
        match &self.board {
            Some(b) => Ok(b.clone()),
            None => Ok(Arc::new(board(c)?)),
        }
    }
}

/// The few Platform fields the masternode list carries per evonode: credits last epoch and over 30
/// days, blocks this epoch, and when it proposes next (`in_sets` null: not known lately).
pub fn list_fields(c: &Connection, view: &View) -> Result<HashMap<String, Value>> {
    let b = view.board(c)?;
    let schedule = view.schedule();
    Ok(registered_evonodes(c)?
        .into_iter()
        .map(|p| {
            let v = json!({
                "last_epoch": b.last.get(&p).map_or(0, |x| x.1),
                "d30": b.d30.get(&p).copied().unwrap_or(0),
                "epoch_blocks": b.epoch_blocks.get(&p).copied().unwrap_or(0),
                "next": schedule.and_then(|s| s.eta(&p)),
                "in_sets": schedule.map(|s| s.slots.contains_key(&p)),
            });
            (p, v)
        })
        .collect())
}

/// The `/platform` document: the current epoch, finalized epochs, the upgrade vote, the proposer
/// schedule, network returns, and one row per evonode.
pub fn network_json(c: &Connection, view: &View, net: Network) -> Result<Value> {
    let b = view.board(c)?;
    let schedule = view.schedule();
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
        epoch_secs(net) as i64
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
        "returns": network_returns(c, &b)?,
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
fn network_returns(c: &Connection, b: &Board) -> Result<Value> {
    let (monthly, basis) = (&b.monthly, b.monthly_basis);
    let mut m: Vec<i64> = monthly.values().copied().collect();
    m.sort_unstable();
    let (yields, span) = platform_yields(c, db::now() - 90 * DAY)?;
    Ok(json!({
        "median_monthly": quantile(&m, 0.5), "monthly_basis_days": basis, "evonodes_paid": m.len(),
        "median_platform_yield": median4(yields.into_values().collect()), "yield_span_days": span,
    }))
}

/// An evonode's Platform rewards for its page.
pub fn mn_json(c: &Connection, protx: &str, view: &View, window_days: i64) -> Result<Value> {
    let b = view.board(c)?;
    let schedule = view.schedule();
    let now = b.now.clone();
    let start = epoch_start(now.as_ref());
    let Some(max_epoch) = latest_finalized(c)? else { return Ok(Value::Null) };
    let from = max_epoch - EPOCHS_SHOWN + 1;
    // Each epoch's reward and its rank by credits among the epoch's proposers.
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
    let mine30 = b.d30.get(protx).copied().unwrap_or(0);
    let all30 = &b.d30_enabled;
    let below = all30.partition_point(|&x| x < mine30);
    // The current epoch so far.
    let cur_epoch = now.as_ref().and_then(|n| n["epoch"].as_i64());
    let total = b.epoch_total;
    let mine = b.epoch_blocks.get(protx).copied().unwrap_or(0);
    let rank = b.epoch_blocks.values().filter(|&&n| n > mine).count() + 1;
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
                t["expected_blocks"] = json!(round_to(share * total as f64, 1));
            }
            json!({
                "epoch": e, "blocks": mine, "blocks_total": total, "proposers": b.epoch_blocks.len(), "rank": (mine > 0).then_some(rank),
                "accrued": accrued, "pool_so_far": pool, "turns": t,
            })
        }
        None => Value::Null,
    };
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
    Ok(json!({
        "first_epoch": first_epoch,
        "epochs": epochs,
        "totals": {
            "last_epoch": c.query_row("SELECT credits FROM platform_payouts WHERE protx = ?1 AND epoch = ?2", params![protx, max_epoch], |r| r.get::<_, i64>(0)).optional()?.unwrap_or(0),
            "last_epoch_index": max_epoch,
            "d30": sum_since(30)?, "d90": sum_since(90)?, "d365": sum_since(365)?, "lifetime": lifetime, "epochs_paid": paid_epochs,
        },
        "d30_percentile": (!all30.is_empty()).then(|| round_to(below as f64 / all30.len() as f64, 3)),
        "d30_of": all30.len(),
        "monthly": b.monthly.get(protx).copied().unwrap_or(0), "monthly_basis_days": b.monthly_basis,
        "annual_yield": platform_yield(c, protx, db::now() - window_days * DAY)?.map(round4),
        "current": current,
        "watched": watched_json(c, protx)?,
        "turns_30d": turns(last30, cur_epoch.unwrap_or(0))?,
        "next": schedule.and_then(|s| s.slots.get(protx).map(|slot| json!({"slots": slot, "eta": s.eta(protx), "sampled": s.ts}))),
        "in_validator_sets": schedule.map(|s| s.slots.contains_key(protx)),
        "balance": balance.map(|(credits, ts)| json!({"credits": credits, "ts": ts})),
        "balance_history": history,
        "vote": vote.map(|(v, ts)| json!({"version": v, "ts": ts})),
        "explorer": explorer_json(c, protx)?,
    }))
}

/// Platform Explorer's lifetime total for an evonode (third party), against our own total over the
/// epochs it had counted when asked. It reads the same epoch records, so the two should match;
/// `before_ours` is anything it counts beyond them.
fn explorer_json(c: &Connection, protx: &str) -> Result<Value> {
    let row: Option<(Option<i64>, Option<i64>, i64)> = c
        .query_row("SELECT total_reward, epoch, fetched FROM platform_explorer WHERE protx = ?1", [protx], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?;
    let Some((total, epoch, fetched)) = row else { return Ok(Value::Null) };
    let ours: i64 = match epoch {
        Some(e) => c
            .query_row(
                "SELECT sum(credits) FROM platform_payouts WHERE protx = ?1 AND epoch < ?2",
                params![protx, e],
                |r| r.get::<_, Option<i64>>(0),
            )?
            .unwrap_or(0),
        None => 0,
    };
    Ok(json!({"total_reward": total, "epoch": epoch, "fetched": fetched, "ours_to_then": ours,
        "before_ours": total.map(|t| (t - ours).max(0))}))
}

/// The finalized epochs we watched in full enough to say how many blocks the evonode should have made.
fn watched_json(c: &Connection, protx: &str) -> Result<Vec<Value>> {
    Ok(query_all(
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
            let coverage = samples as f64 * agreed_interval() / secs.max(1) as f64;
            Ok(json!({
                "epoch": r.get::<_, i64>(0)?, "expected": (all > 0).then(|| round_to(mem as f64 / all as f64 * total as f64, 1)),
                "actual": actual, "turns": r.get::<_, i64>(5)?, "missed": r.get::<_, i64>(6)?,
                "coverage": round_to(coverage.min(1.0), 2),
            }))
        },
    )?)
}

/// Seconds per epoch, for turning "30 days" into epochs: the latest finalized one's, or the network's.
fn epoch_secs_hint(c: &Connection, now: &Option<Value>) -> i64 {
    latest_span(c)
        .ok()
        .flatten()
        .map(|(_, secs)| secs)
        .or_else(|| {
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
        assert_eq!(core_rewards(1, 150, 150), Some(112_500_000_000 * 150));
        let second = 112_500_000_000 - 112_500_000_000 / 14;
        assert_eq!(core_rewards(150, 155, 150), Some(112_500_000_000 + 5 * second));
        assert_eq!(core_rewards(10, 9, 150), Some(0));
        // Mainnet, block 2,563,677 (year 12): 0.46231323 DASH a block went into the credit pool.
        assert_eq!(core_rewards(2_563_677, 2_563_677, CORE_HALVING).unwrap() / 1000, 46_231_323);
    }

    #[test]
    fn core_rewards_stop_where_drive_does() {
        // Drive's table has 200 years; its heights are u32. Anything past either has no answer (and
        // returns at once rather than folding year after year).
        assert!(core_rewards(1, 200 * CORE_HALVING, CORE_HALVING).is_some());
        assert_eq!(core_rewards(1, 200 * CORE_HALVING + 2, CORE_HALVING), None);
        assert_eq!(core_rewards(1, u64::from(u32::MAX) + 1, CORE_HALVING), None);
        assert_eq!(core_rewards(1, u64::MAX, CORE_HALVING), None);
        assert_eq!(pool_so_far(&json!({"first_core_height": 2_564_064u64, "core_height": u64::MAX})), None);
        assert_eq!(pool_so_far(&json!({"first_core_height": u64::MAX, "core_height": u64::MAX})), None);
        assert!(pool_so_far(&json!({"first_core_height": 2_564_064u64, "core_height": 2_564_718u64})).is_some());
    }

    #[test]
    fn metadata_counts_only_when_both_answers_agree() {
        let a = Meta { height: 100, core_height: 2_000, epoch: 87, time_ms: 5_000, protocol_version: 13 };
        // A block apart: the lower of each.
        let b = Meta { height: 101, core_height: 2_001, time_ms: 6_000, ..a };
        assert_eq!(agreed_meta(&a, &b), Some(a));
        assert_eq!(agreed_meta(&b, &a), Some(a));
        // One node copying an honest answer can't move the chain state.
        for fake in [
            Meta { height: u64::MAX, ..a },
            Meta { core_height: u64::MAX, ..a },
            Meta { height: 103, ..a },
            Meta { epoch: 60_000, ..a },
            Meta { protocol_version: 99, ..a },
        ] {
            assert_eq!(agreed_meta(&a, &fake), None, "{fake:?}");
        }
        // A time far ahead from one side gives way to the other's.
        assert_eq!(agreed_meta(&a, &Meta { time_ms: u64::MAX, ..a }).map(|m| m.time_ms), Some(5_000));
    }

    #[test]
    fn peers_are_one_per_ip() {
        let addrs: Vec<String> =
            ["37.27.83.17:443", "37.27.83.17:1443", "10.0.0.1:443", "65.21.1.1:443"].map(String::from).to_vec();
        let ips: Vec<String> = distinct_ips(&addrs).iter().map(ToString::to_string).collect();
        assert_eq!(ips, ["37.27.83.17:443", "65.21.1.1:443"]);
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
    fn schedule_from_one_of_the_two_oldest_visits_every_rotating_set() {
        // Twelve sets, the current one (11) among the two oldest: Drive goes on to set 0, then through
        // all ten that rotate. None is left without a turn.
        let order: Vec<Vec<u8>> = (0..12).map(|i| vec![i]).collect();
        let sets: Vec<ValidatorSet> = (0..12).map(|i| set(i, &[(i + 100, false)])).collect();
        let q = Quorums { order, current: vec![11], sets, last_proposer: id(111) };
        let s = schedule(&q);
        assert_eq!(s.iter().map(|x| x.0).collect::<Vec<_>>(), (100..110).map(id).collect::<Vec<_>>());
        assert_eq!(s.last(), Some(&(id(109), 10)));
    }

    #[test]
    fn turns_need_a_plausible_gap() {
        let s = set(1, &[(1, false), (2, false), (3, false), (4, false)]);
        let q = |last: u8| Quorums {
            order: vec![vec![1]],
            current: vec![1],
            sets: vec![s.clone()],
            last_proposer: id(last),
        };
        let m = |height: u64, time_ms: u64| Meta { height, core_height: 0, epoch: 87, time_ms, protocol_version: 13 };
        let prev = (q(1), m(100, 1_000));
        // 1 → 4 in one block: 2 and 3 missed.
        assert_eq!(turns_since(&prev, &q(4), &m(101, 2_000)).iter().filter(|t| t.1).count(), 2);
        // More blocks than the set has members, no time passed, or another epoch: nothing is pinned.
        assert!(turns_since(&prev, &q(4), &m(105, 2_000)).is_empty());
        assert!(turns_since(&prev, &q(4), &m(101, 1_000)).is_empty());
        assert!(turns_since(&prev, &q(4), &Meta { epoch: 88, ..m(101, 2_000) }).is_empty());
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

    /// Recorded epoch 86, with what came before it (85, and 34, mainnet's oldest stored), and our tip,
    /// clock and mainnet's epoch length after it.
    fn epoch_86() -> (Finalized, Before, Before, Limits) {
        let (epochs, _) = parse_finalized(&recorded(include_bytes!("testdata/finalized-86.bin"))).unwrap();
        let e = epochs[0].clone();
        let before = Before {
            epoch: 85,
            first_block_time: e.first_block_time_ms / 1000 - 788_400,
            end_block: e.first_block_height,
            next_core_height: e.first_core_height - 1,
        };
        let oldest =
            Before { epoch: 34, first_block_time: 1_751_601_268, end_block: 206_380, next_core_height: 2_303_950 };
        let limits = Limits {
            tip: Some(e.next_core_height + 600),
            now: e.first_block_time_ms / 1000 + 20 * 86_400,
            epoch_secs: epoch_secs(Network::Mainnet),
        };
        (e, before, oldest, limits)
    }

    /// The current epoch after it, as recorded the same day.
    fn epoch_87() -> (Epoch, Meta) {
        let (epochs, meta) = parse_epochs(&recorded(include_bytes!("testdata/epochs-current.bin"))).unwrap();
        (epochs[0], meta)
    }

    #[test]
    fn finalized_epochs_follow_drives_rules() {
        let (e, before, oldest, limits) = epoch_86();
        let (current, _) = epoch_87();
        let one = std::slice::from_ref(&e);
        assert!(check_finalized(one, 86..=86, Some(before), Some(oldest), Some(&current), limits).is_ok());
        assert!(check_finalized(one, 34..=133, None, None, None, Limits { tip: None, ..limits }).is_ok());
        assert!(check_finalized(&[], 86..=185, Some(before), Some(oldest), None, limits).is_ok());
        // Core block rewards are exactly what Drive pays for the epoch's Core blocks.
        assert_eq!(core_rewards(e.first_core_height, e.next_core_height, CORE_HALVING), Some(e.core_rewards));
        let reject = |f: &dyn Fn(&mut Finalized), range: RangeInclusive<u64>, before: Before, limits: Limits| {
            let mut bad = e.clone();
            f(&mut bad);
            check_finalized(&[bad], range, Some(before), Some(oldest), None, limits).unwrap_err().to_string()
        };
        // Not asked for: an epoch far ahead would stop collection for good.
        assert!(reject(&|x| x.epoch = 60_000, 86..=185, before, limits).contains("asked"));
        assert!(reject(&|_| {}, 87..=186, before, limits).contains("asked"));
        // Past Drive's u16 epoch indexes.
        assert!(reject(&|x| x.epoch = 70_000, 0..=100_000, before, limits).contains("index"));
        // Blocks that don't add up, a proposer twice, or none.
        assert!(reject(&|x| x.proposers[0].1 += 1, 86..=86, before, limits).contains("blocks"));
        assert!(reject(&|x| x.proposers[1].0 = x.proposers[0].0, 86..=86, before, limits).contains("twice"));
        let none = |x: &mut Finalized| {
            x.proposers.clear();
            x.total_blocks = 0;
        };
        assert!(reject(&none, 86..=86, before, limits).contains("no blocks"));
        // Made-up rewards, or a pool no ledger holds.
        assert!(reject(&|x| x.core_rewards += 1, 86..=86, before, limits).contains("Core block rewards"));
        assert!(reject(&|x| x.processing_fees = u64::MAX, 86..=86, before, limits).contains("pool"));
        let gap_max = Before { next_core_height: u64::MAX, ..before };
        assert!(reject(&|_| {}, 86..=86, gap_max, limits).contains("after the one before"));
        // Core heights past our tip.
        assert!(
            reject(&|_| {}, 86..=86, before, Limits { tip: Some(e.next_core_height - 1), ..limits }).contains("tip")
        );
        // Times: in the future, or not after the epoch before.
        let future = |x: &mut Finalized| x.first_block_time_ms = (limits.now + 3_600) * 1000;
        assert!(reject(&future, 86..=86, before, limits).contains("future"));
        let late = Before { first_block_time: e.first_block_time_ms / 1000, ..before };
        assert!(reject(&|_| {}, 86..=86, late, limits).contains("later"));
        // Not starting where the one before ended (an epoch left out), in Core or Platform blocks.
        let gap = Before { next_core_height: before.next_core_height - 5, ..before };
        assert!(reject(&|_| {}, 86..=86, gap, limits).contains("after the one before"));
        let gap = Before { end_block: before.end_block - 1, ..before };
        assert!(reject(&|_| {}, 86..=86, gap, limits).contains("starts at block"));
        // Numbered other than its start time says: from the one before, or (drifting a little each
        // epoch) from the oldest stored.
        let early = Before { first_block_time: before.first_block_time - 788_401, ..before };
        assert!(reject(&|_| {}, 86..=86, early, limits).contains("out of step"));
        let drifted = Before { epoch: 30, ..oldest };
        assert!(
            check_finalized(one, 86..=86, Some(before), Some(drifted), None, limits)
                .unwrap_err()
                .to_string()
                .contains("out of step")
        );
        // Out of order within a page.
        let mut next = e.clone();
        next.epoch = 85;
        assert!(
            check_finalized(&[e.clone(), next], 80..=90, None, None, None, limits)
                .unwrap_err()
                .to_string()
                .contains("order")
        );
    }

    #[test]
    fn the_last_finished_epoch_ends_where_the_current_one_starts() {
        let (e, before, oldest, limits) = epoch_86();
        let (current, _) = epoch_87();
        let check = |e: &Finalized, current: &Epoch| {
            check_finalized(std::slice::from_ref(e), 86..=86, Some(before), Some(oldest), Some(current), limits)
        };
        assert!(check(&e, &current).is_ok());
        // Two evonodes moving the last epoch's end (with Core block rewards to match) would leave every
        // true epoch after it failing to follow.
        let mut longer = e.clone();
        longer.next_core_height += 10;
        longer.core_rewards = core_rewards(longer.first_core_height, longer.next_core_height, CORE_HALVING).unwrap();
        assert!(
            check_finalized(std::slice::from_ref(&longer), 86..=86, Some(before), Some(oldest), None, limits).is_ok()
        );
        assert!(check(&longer, &current).unwrap_err().to_string().contains("ends at"));
        let mut fewer = e.clone();
        fewer.total_blocks -= 1;
        fewer.proposers[0].1 -= 1;
        assert!(check(&fewer, &current).is_err());
        // Or a current epoch not after it in time.
        let late = Epoch { start_ms: current.start_ms + 2 * 788_400_000, ..current };
        assert!(check(&e, &late).is_err());
        // With nothing new to fetch, the newest stored epoch is held to the same.
        assert!(check_ties(&Before::of(&e), &current, limits.epoch_secs).is_ok());
        assert!(check_ties(&Before::of(&longer), &current, limits.epoch_secs).is_err());
    }

    #[test]
    fn a_made_up_current_epoch_doesnt_follow_the_stored_ones() {
        let (e, _, oldest, _) = epoch_86();
        let (current, meta) = epoch_87();
        let newest = Before::of(&e);
        let d = epoch_secs(Network::Mainnet);
        let now = current.start_ms / 1000 + 86_400;
        assert!(check_current(&current, &meta, now).is_ok());
        assert!(check_current_follows(&current, Some(&newest), Some(&oldest), d).is_ok());
        assert!(check_current_follows(&current, None, None, d).is_ok());
        // Far ahead of the stored epochs, started now: its number doesn't match its start.
        let ahead = Epoch { epoch: 60_000, start_ms: now * 1000, ..current };
        assert!(check_current(&ahead, &Meta { epoch: 60_000, ..meta }, now).is_ok());
        assert!(check_current_follows(&ahead, Some(&newest), Some(&oldest), d).is_err());
        // Past u16, in the future, or past the chain-locked height of its own answer.
        assert!(check_current(&Epoch { epoch: 70_000, ..current }, &Meta { epoch: 70_000, ..meta }, now).is_err());
        assert!(check_current(&current, &meta, current.start_ms / 1000 - 3_600).is_err());
        assert!(check_current(&current, &Meta { epoch: 88, ..meta }, now).is_err());
        assert!(check_current(&current, &Meta { core_height: current.first_core_height - 1, ..meta }, now).is_err());
        // Not after the newest stored, or starting before it ended.
        assert!(check_current_follows(&current, Some(&Before { epoch: 87, ..newest }), Some(&oldest), d).is_err());
        let early = Epoch { first_core_height: newest.next_core_height - 1, ..current };
        assert!(check_current_follows(&early, Some(&newest), Some(&oldest), d).is_err());
    }

    #[test]
    fn core_height_is_held_to_our_tip() {
        assert_eq!(held_core_height(2_564_733, Some(2_564_700)), 2_564_700);
        assert_eq!(held_core_height(2_564_733, Some(2_564_800)), 2_564_733);
        assert_eq!(held_core_height(u64::MAX, None), u64::MAX);
    }

    /// Testnet epochs as stored (epoch, first block, start, first Core block, next epoch's Core start,
    /// blocks, processing fees, storage fees, Core block rewards, protocol version): the chain halted
    /// between 7552 and 8592, and 9078's chain-locked height didn't move.
    const TESTNET: [[u64; 10]; 8] = [
        [7552, 152_132, 1_748_540_423, 1_260_352, 1_286_852, 24, 15_506_760, 35_668_762, 1_917_243_689_584_638, 8],
        [8592, 152_156, 1_752_284_470, 1_286_853, 1_286_876, 20, 0, 5_625_496, 1_730_834_981_136, 9],
        [8593, 152_176, 1_752_288_027, 1_286_877, 1_286_901, 20, 0, 5_615_173, 1_802_953_105_350, 9],
        [9076, 161_877, 1_754_026_839, 1_299_162, 1_299_264, 20, 0, 24_428_935, 7_428_166_794_042, 9],
        [9077, 161_897, 1_754_030_495, 1_299_265, 1_299_276, 20, 3_089_529, 24_425_791, 865_417_490_568, 9],
        [9078, 161_917, 1_754_034_112, 1_299_277, 1_299_276, 20, 0, 24_389_067, 0, 9],
        [9079, 161_937, 1_754_037_768, 1_299_277, 1_299_318, 19, 0, 24_369_926, 3_028_961_216_988, 9],
        [9080, 161_956, 1_754_041_210, 1_299_319, 1_299_341, 20, 0, 24_368_880, 1_658_716_856_922, 9],
    ];

    /// A stored testnet epoch, its blocks split between two made-up proposers.
    fn testnet(n: u64) -> Finalized {
        let r = TESTNET.iter().find(|r| r[0] == n).unwrap();
        Finalized {
            epoch: r[0],
            first_block_height: r[1],
            first_block_time_ms: r[2] * 1000 + 417,
            first_core_height: r[3],
            next_core_height: r[4],
            total_blocks: r[5],
            processing_fees: r[6],
            storage_fees: r[7],
            core_rewards: r[8],
            protocol_version: r[9],
            proposers: vec![(id(1), r[5] / 2), (id(2), r[5] - r[5] / 2)],
        }
    }

    fn testnet_limits() -> Limits {
        Limits { tip: Some(1_400_000), now: 1_793_490_900, epoch_secs: epoch_secs(Network::Testnet) }
    }

    #[test]
    fn honest_pages_pass_across_halts_and_still_chain_locks() {
        let limits = testnet_limits();
        // One page across the halt (epochs 7553–8591 never happened), then one with an epoch over which
        // the chain-locked height didn't move: it starts after it and ends at it, with no Core rewards.
        let page1: Vec<Finalized> = [7552, 8592, 8593].map(testnet).to_vec();
        check_finalized(&page1, 7552..=8651, None, None, None, limits).unwrap();
        let still = testnet(9078);
        assert_eq!(still.first_core_height, still.next_core_height + 1);
        assert_eq!(core_rewards(still.first_core_height, still.next_core_height, CORE_HALVING), Some(0));
        let page2: Vec<Finalized> = (9076..=9080).map(testnet).collect();
        let oldest = Some(Before::of(&page1[0]));
        assert!(check_finalized(&page2, 9076..=9080, None, oldest, None, limits).is_ok());
        // The next epoch after it follows on as usual, and so does the current one after the page.
        let before = Some(Before::of(&page2[2]));
        assert!(check_finalized(&page2[3..], 9079..=9080, before, oldest, None, limits).is_ok());
        let current = Epoch {
            epoch: 9081,
            first_block_height: 161_976,
            first_core_height: 1_299_341,
            start_ms: 1_754_044_810_000,
            protocol_version: 9,
        };
        assert!(check_finalized(&page2, 9076..=9080, None, oldest, Some(&current), limits).is_ok());
        assert!(check_current_follows(&current, before.as_ref(), oldest.as_ref(), limits.epoch_secs).is_ok());
        // A Core start more than one past the end is still turned down.
        let mut back = still.clone();
        back.first_core_height += 1;
        assert!(check_finalized(&[back], 9078..=9078, None, None, None, limits).is_err());
    }

    #[test]
    fn restart_needs_a_later_chain_three_rounds_in_a_row() {
        let stored =
            Before { epoch: 20_037, first_block_time: 1_793_486_429, end_block: 655_474, next_core_height: 1_583_171 };
        let new_chain = Epoch { epoch: 3, start_ms: (stored.first_block_time + 7_200) * 1000, ..Epoch::default() };
        let mut behind = 0;
        let seen = |net, e: &Epoch, behind: &mut u32| restart(net, e, Some(&stored), behind);
        assert_eq!(seen(Network::Testnet, &new_chain, &mut behind), Restart::Pending);
        assert_eq!(seen(Network::Testnet, &new_chain, &mut behind), Restart::Pending);
        // A round that sees the stored chain again starts the count over.
        let same = Epoch { epoch: 20_038, start_ms: (stored.first_block_time + 3_600) * 1000, ..Epoch::default() };
        assert_eq!(seen(Network::Testnet, &same, &mut behind), Restart::Go);
        assert_eq!(behind, 0);
        for _ in 0..2 {
            assert_eq!(seen(Network::Testnet, &new_chain, &mut behind), Restart::Pending);
        }
        assert_eq!(seen(Network::Testnet, &new_chain, &mut behind), Restart::Clear);
        assert_eq!(behind, 0);
        // Evonodes that halted at an older epoch started it before the stored one: lag, however long.
        let stuck = Epoch { epoch: 20_030, start_ms: (stored.first_block_time - 7 * 3_600) * 1000, ..Epoch::default() };
        for _ in 0..10 {
            assert_eq!(seen(Network::Testnet, &stuck, &mut behind), Restart::Lag);
        }
        // Within the margin, nothing to say.
        let near = Epoch { epoch: 20_036, ..stuck };
        assert_eq!(seen(Network::Testnet, &near, &mut behind), Restart::Go);
        // Mainnet never clears.
        for _ in 0..10 {
            assert_eq!(seen(Network::Mainnet, &new_chain, &mut behind), Restart::Lag);
        }
        assert_eq!(restart(Network::Testnet, &new_chain, None, &mut behind), Restart::Go);
    }

    #[test]
    fn a_wrong_stored_tail_is_found_and_dropped() {
        let mut c = scratch_db("recover");
        let honest: Vec<Finalized> = (9076..=9080).map(testnet).collect();
        // Two evonodes made 9080 end later than it did: no true epoch follows it.
        let mut stored = honest.clone();
        stored[4].next_core_height += 10;
        store_finalized(&mut c, &stored).unwrap();
        let read = newest_stored(&c, PAGE).unwrap();
        assert_eq!(read.iter().map(|e| e.epoch).collect::<Vec<_>>(), (9076..=9080).collect::<Vec<_>>());
        let at = Meta { epoch: 9_081, time_ms: 1_754_045_000_000, ..Meta::default() };
        // What's stored reads back as it was given (times to the second).
        assert_eq!(first_wrong(&read, &stored, &at), None);
        assert_eq!(first_wrong(&read, &honest, &at), Some(9080));
        drop_epochs_from(&mut c, 9080).unwrap();
        let count = |t: &str| {
            c.query_row(&format!("SELECT count(*) FROM {t} WHERE epoch >= 9080"), [], |r| r.get::<_, i64>(0)).unwrap()
        };
        assert_eq!((count("platform_epochs"), count("platform_payouts")), (0, 0));
        assert_eq!(stored_ends(&c).unwrap().0.map(|b| b.epoch), Some(9079));
        // An epoch that never happened: dropped once the answering chain is past it, in epochs or time.
        assert_eq!(first_wrong(&read, &honest[..4], &at), Some(9080));
        let lagging = Meta { epoch: 9_079, time_ms: 1_754_041_000_000, ..at };
        assert_eq!(first_wrong(&read, &honest[..3], &lagging), None);
        let later = Meta { epoch: 9_079, time_ms: 1_754_045_000_000, ..at };
        assert_eq!(first_wrong(&read, &honest[..3], &later), Some(9080));
        // An epoch it has that wasn't stored: everything after it goes.
        assert_eq!(first_wrong(&[read[0].clone(), read[2].clone()], &honest[..3], &at), Some(9077));
        // Rounds that fail at the same stored epoch add up; moving on starts over.
        let mut p = Progress::default();
        p.stalled_at(Some(9080));
        p.stalled_at(Some(9080));
        assert_eq!(p.stalled, (Some(9080), 2));
        p.stalled_at(Some(9081));
        assert_eq!(p.stalled, (Some(9081), 1));
    }

    /// A database with the app's schema, in a temporary file.
    fn scratch_db(name: &str) -> Connection {
        let dir = std::env::temp_dir().join(format!("dash-nodes-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("r.sqlite");
        let _ = std::fs::remove_file(&path);
        drop(db::Db::open(&path).unwrap());
        Connection::open(&path).unwrap()
    }

    #[test]
    fn history_is_pruned_and_a_reset_clears_everything() {
        let mut c = scratch_db("prune");
        let now = db::now();
        let old = now - 40 * DAY;
        c.execute_batch(&format!(
            "INSERT INTO platform_epochs VALUES (10, 1, {old}, 0, 0, 10, 0, 0, 0, 13, 1), (11, 2, {}, 0, 0, 10, 0, 0, 0, 13, 1);
             INSERT INTO platform_payouts VALUES (10, 'a', 10, 5), (11, 'a', 10, 5);
             INSERT INTO platform_turns VALUES (10, 'a', 1, 1, 0), (11, 'a', 1, 1, 0);
             INSERT INTO platform_turn_samples VALUES (10, 1, 1), (11, 1, 1);
             -- 'a' changed twice before the window and once in it; 'b' only long ago.
             INSERT INTO platform_balance_history VALUES ('a', {old}, 1), ('a', {}, 2), ('a', {}, 3), ('b', {old}, 7);
             INSERT INTO meta VALUES ('platform_now', '{{}}');",
            now - DAY,
            old + 1,
            now - DAY,
        ))
        .unwrap();
        // Mainnet keeps all of it.
        prune(&c, Network::Mainnet, now).unwrap();
        let count = |c: &Connection, t: &str| {
            c.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get::<_, i64>(0)).unwrap()
        };
        assert_eq!((count(&c, "platform_turns"), count(&c, "platform_balance_history")), (2, 4));
        // Testnet keeps a month: epoch 10's turns go, and every evonode keeps its latest balance.
        prune(&c, Network::Testnet, now).unwrap();
        assert_eq!((count(&c, "platform_turns"), count(&c, "platform_turn_samples")), (1, 1));
        let kept: Vec<(String, i64)> =
            query_all(&c, "SELECT protx, credits FROM platform_balance_history ORDER BY protx, ts", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(kept, [("a".to_owned(), 3), ("b".to_owned(), 7)]);
        // A testnet reset clears every table and the current epoch.
        clear_platform(&mut c).unwrap();
        for t in PLATFORM_TABLES {
            assert_eq!(count(&c, t), 0, "{t}");
        }
        assert_eq!(current(&c).unwrap(), None);
    }

    #[test]
    fn decodes_recorded_live_answers() {
        let (epochs, meta) = parse_epochs(&recorded(include_bytes!("testdata/epochs-current.bin"))).unwrap();
        assert_eq!(epochs[0].epoch, 87);
        assert_eq!(epochs[0].start_ms / 1000, 1_793_386_337);
        assert_eq!((epochs[0].first_block_height, epochs[0].first_core_height), (461_783, 2_564_064));
        assert!(meta.height > epochs[0].first_block_height && meta.core_height > epochs[0].first_core_height);
        // A real answer passes the checks on the current epoch (more in a_made_up_current_epoch_doesnt_follow_the_stored_ones).
        assert!(check_current(&epochs[0], &meta, epochs[0].start_ms / 1000 + 86_400).is_ok());
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

    /// A getCurrentQuorumsInfo answer: the rotation `order`, and sets of (quorum hash, members).
    fn quorums_answer(order: &[&[u8]], sets: &[(&[u8], usize)]) -> Vec<u8> {
        let mut v0 = Vec::new();
        for h in order {
            put_bytes(&mut v0, 1, h);
        }
        put_bytes(&mut v0, 2, sets.first().map_or(&[][..], |s| s.0));
        for &(hash, n) in sets {
            let mut s = Vec::new();
            put_bytes(&mut s, 1, hash);
            for i in 0..n {
                let mut m = Vec::new();
                let mut pro_tx = [0u8; 32];
                pro_tx[..8].copy_from_slice(&(i as u64).to_be_bytes());
                put_bytes(&mut m, 1, &pro_tx);
                put_bytes(&mut s, 3, &m);
            }
            put_bytes(&mut v0, 3, &s);
        }
        put_bytes(&mut v0, 4, &id(0));
        let mut resp = Vec::new();
        put_bytes(&mut resp, 1, &v0);
        resp
    }

    #[test]
    fn hostile_validator_sets_are_cheap() {
        let start = Instant::now();
        // ~1 MB: 250k repeats of one (empty) quorum hash pointing at a set of 14k members. Walked as
        // sent, that's 3.5e9 steps; it's turned down instead.
        let repeats = vec![&[][..]; 250_000];
        let err = parse_quorums(&quorums_answer(&repeats, &[(&[], 14_000)])).unwrap_err();
        assert!(err.to_string().contains("members"), "{err:#}");
        // Within the caps, repeats count once.
        let (q, _) = parse_quorums(&quorums_answer(&repeats, &[(&[], MAX_MEMBERS)])).unwrap();
        assert_eq!((q.order.len(), schedule(&q).len()), (1, MAX_MEMBERS - 1));
        // Too many sets, in the answer or in the rotation.
        let hashes: Vec<[u8; 1]> = (0..=MAX_SETS as u8).map(|i| [i]).collect();
        let many: Vec<(&[u8], usize)> = hashes.iter().map(|h| (&h[..], 1)).collect();
        assert!(parse_quorums(&quorums_answer(&[], &many)).is_err());
        let order: Vec<&[u8]> = hashes.iter().map(|h| &h[..]).collect();
        assert!(parse_quorums(&quorums_answer(&order, &many[..1])).is_err());
        // The schedule itself doesn't trust the order either.
        let big = set(0, &[]);
        let members = (0..14_000u32)
            .map(|i| {
                let mut p = [0u8; 32];
                p[..4].copy_from_slice(&i.to_be_bytes());
                Member { pro_tx_hash: p, banned: false }
            })
            .collect();
        let q = Quorums {
            order: vec![vec![0]; 250_000],
            current: vec![0],
            sets: vec![ValidatorSet { members, ..big }],
            last_proposer: [0; 32],
        };
        assert_eq!(schedule(&q).len(), 13_999);
        assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
    }

    #[test]
    fn explorer_fields() {
        let v = json!({"totalReward": 36_235_244_983_446u64, "epochInfo": {"number": 87}});
        assert_eq!(parse_explorer(&v), (Some(36_235_244_983_446), Some(87)));
        assert_eq!(parse_explorer(&json!({"totalReward": "12", "epochInfo": null})), (Some(12), None));
        assert_eq!(parse_explorer(&json!({"message": "not found"})), (None, None));
    }
}

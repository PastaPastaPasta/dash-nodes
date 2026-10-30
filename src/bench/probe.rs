//! One bench test against one node: connect, handshake, ping, download recent headers, ask for the
//! full masternode list, grade. Every step is bounded by the test's deadline, so a slow or hostile
//! node can't hold a slot; whatever was measured before something failed is still reported.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use super::grade::{self, Grade, Metrics, round1, round3};
use super::wire::{self, Hash, RawHeader};
use crate::addr::PeerAddr;
use crate::network::Network;
use crate::p2p::{self, PROTOCOL_VERSION, Timeouts};
use crate::reference::Reference;
use crate::transport::{self, Route};

/// Largest message accepted, as in Dash Core (MAX_PROTOCOL_MESSAGE_LENGTH, 3 MiB). A full mainnet
/// `mnlistdiff` from genesis is well under it.
const MAX_MESSAGE: usize = 3 << 20;
/// How long one ping may wait for its pong, and for all pings together.
const PING_TIMEOUT: Duration = Duration::from_secs(5);
const PING_PHASE: Duration = Duration::from_secs(15);
/// Re-requests after a stall before the headers download is given up.
const MAX_STALLS: u32 = 3;
/// Time kept back from the headers download for the masternode list diff.
const MNLISTDIFF_RESERVE: Duration = Duration::from_secs(15);
const MNLISTDIFF_TIMEOUT: Duration = Duration::from_secs(30);
/// Masternode list sizes within this fraction of the reference list count as plausible.
const MNLIST_TOLERANCE: f64 = 0.1;

/// A reference document with its hashes decoded, ready for checking headers.
#[derive(Clone, Debug)]
pub struct CheckedReference {
    pub tip: u32,
    pub tip_hash: Hash,
    pub hashes: HashMap<u32, Hash>,
}

impl CheckedReference {
    pub fn new(r: &Reference) -> Result<CheckedReference> {
        let tip_hash = wire::parse_hash_hex(&r.tip_hash).ok_or_else(|| anyhow!("bad tip_hash"))?;
        let mut hashes = HashMap::new();
        for (&h, hash) in &r.hashes {
            hashes.insert(h, wire::parse_hash_hex(hash).ok_or_else(|| anyhow!("bad hash at {h}"))?);
        }
        if hashes.get(&r.tip) != Some(&tip_hash) {
            bail!("reference doesn't include its tip");
        }
        Ok(CheckedReference { tip: r.tip, tip_hash, hashes })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// Headers to download, ending at the reference tip; a multiple of 2000 so every batch ends on
    /// a reference height.
    pub depth: u32,
    /// Without a byte from the node for this long, the headers request is re-sent (a stall).
    pub stall: Duration,
    /// The whole test, connect to grade.
    pub limit: Duration,
    pub pings: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PingStats {
    pub min: Option<f64>,
    pub median: Option<f64>,
    pub max: Option<f64>,
    pub samples: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HeaderStats {
    pub start_height: u32,
    pub requested: u32,
    pub served: u32,
    pub valid: bool,
    pub seconds: f64,
    pub per_sec: f64,
    pub bytes: u64,
    pub stalls: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MnListDiffStats {
    pub bytes: u64,
    pub seconds: f64,
    pub entries: Option<usize>,
    pub ok: bool,
}

/// One test's outcome: the document format the Worker serves (see README, bench).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BenchResult {
    pub ts: i64,
    pub vantage: String,
    pub address: String,
    /// The handshake completed, so the node was measured and graded.
    pub ok: bool,
    /// What went wrong, phases separated by "; " (None when everything worked).
    pub error: Option<String>,
    pub user_agent: Option<String>,
    pub protocol: Option<i32>,
    pub height: Option<i32>,
    pub connect_ms: Option<f64>,
    pub handshake_ms: Option<f64>,
    pub ping_ms: Option<PingStats>,
    pub headers: Option<HeaderStats>,
    pub mnlistdiff: Option<MnListDiffStats>,
    pub grade: Option<Grade>,
}

/// Counts every byte read from peers, for the bandwidth guard.
pub type Meter = Arc<AtomicU64>;

/// Bytes read from the socket at a time.
const READ_CHUNK: usize = 64 * 1024;

/// A connection that answers the peer's pings and counts what it reads. Reads (at most READ_CHUNK
/// at a time) go into a buffer and whole messages are taken from it, so a read cut short by a
/// timeout loses nothing and the connection stays usable. The buffer holds at most one message
/// plus a chunk.
struct Session {
    stream: TcpStream,
    magic: [u8; 4],
    meter: Meter,
    buf: Vec<u8>,
    /// Start of the unread part of `buf`.
    pos: usize,
    chunk: Box<[u8; READ_CHUNK]>,
    /// When the node last sent us something, or we last asked it for something (`expect_reply`):
    /// what a stall is measured from.
    quiet_since: Instant,
    /// Writes don't outlive the test either.
    deadline: Instant,
}

impl Session {
    fn new(stream: TcpStream, net: Network, meter: &Meter, deadline: Instant) -> Session {
        Session {
            stream,
            magic: net.magic(),
            meter: meter.clone(),
            buf: Vec::new(),
            pos: 0,
            chunk: Box::new([0; READ_CHUNK]),
            quiet_since: Instant::now(),
            deadline,
        }
    }

    async fn send(&mut self, cmd: &str, payload: &[u8]) -> Result<()> {
        let msg = p2p::encode(self.magic, cmd, payload);
        let left = self.deadline.saturating_duration_since(Instant::now());
        timeout(left, self.stream.write_all(&msg))
            .await
            .map_err(|_| anyhow!("write timeout"))?
            .map_err(|e| anyhow!("write: {}", e.kind()))?;
        Ok(())
    }

    /// Sends a request whose answer is waited for with an `idle` limit in `recv`.
    async fn request(&mut self, cmd: &str, payload: &[u8]) -> Result<()> {
        self.send(cmd, payload).await?;
        self.quiet_since = Instant::now();
        Ok(())
    }

    /// The next message other than a ping (pings are answered here), before `until`, and with
    /// `idle` set, before the node has been quiet that long. A flood of buffered messages can't
    /// outlast `until` either.
    async fn recv(&mut self, until: Instant, idle: Option<Duration>) -> Result<(String, Vec<u8>)> {
        loop {
            if Instant::now() >= until {
                return Err(Timeout.into());
            }
            match self.take_message()? {
                Some((cmd, payload)) if cmd == "ping" => {
                    // BIP31: echo the nonce. A malformed ping is the peer's problem, not ours.
                    if payload.len() == 8 {
                        self.send("pong", &payload).await?;
                    }
                }
                Some(msg) => return Ok(msg),
                None => {
                    // Drop what's been taken; what's left is part of one message.
                    self.buf.drain(..self.pos);
                    self.pos = 0;
                    let wait = idle.map_or(until, |i| until.min(self.quiet_since + i));
                    let read = self.stream.read(&mut self.chunk[..]);
                    let n = timeout(wait.saturating_duration_since(Instant::now()), read)
                        .await
                        .map_err(|_| Timeout)?
                        .map_err(|e| anyhow!("read: {}", e.kind()))?;
                    if n == 0 {
                        bail!("connection closed");
                    }
                    self.buf.extend_from_slice(&self.chunk[..n]);
                    self.quiet_since = Instant::now();
                    self.meter.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
        }
    }

    /// The next complete message in the buffer, if there is one.
    fn take_message(&mut self) -> Result<Option<(String, Vec<u8>)>> {
        let rest = &self.buf[self.pos..];
        if rest.len() < 24 {
            return Ok(None);
        }
        let (cmd, len) = p2p::decode_header(&rest[..24], self.magic, MAX_MESSAGE)?;
        if rest.len() < 24 + len {
            return Ok(None);
        }
        let payload = rest[24..24 + len].to_vec();
        p2p::verify_checksum(&cmd, &rest[..24], &payload)?;
        self.pos += 24 + len;
        Ok(Some((cmd, payload)))
    }
}

/// A read that ran out of time (as opposed to a closed or broken connection).
#[derive(Debug)]
struct Timeout;

impl std::fmt::Display for Timeout {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("timeout")
    }
}

impl std::error::Error for Timeout {}

fn is_timeout(e: &anyhow::Error) -> bool {
    e.is::<Timeout>()
}

fn ms(d: Duration) -> f64 {
    round1(d.as_secs_f64() * 1000.0)
}

pub struct Target<'a> {
    pub net: Network,
    pub addr: &'a PeerAddr,
    pub route: &'a Route,
    pub timeouts: &'a Timeouts,
    /// Registered masternodes in the source's list, for the mnlistdiff plausibility check.
    pub expected_mns: Option<usize>,
}

/// Runs one test. Never fails: problems end up in `error` (and `ok: false` if the handshake
/// didn't complete).
pub async fn run(
    t: &Target<'_>,
    reference: &CheckedReference,
    p: &Params,
    vantage: &str,
    meter: &Meter,
) -> BenchResult {
    let mut res = BenchResult {
        ts: crate::db::now(),
        vantage: vantage.into(),
        address: t.addr.to_string(),
        ..Default::default()
    };
    let deadline = Instant::now() + p.limit;
    let mut errors = Vec::new();
    // Every read, write and the connect are bounded by `deadline`; this is a backstop.
    if timeout(p.limit + Duration::from_secs(10), run_inner(t, reference, p, deadline, meter, &mut res, &mut errors))
        .await
        .is_err()
    {
        errors.push("test timeout".into());
    }
    if res.ok {
        let headers = res.headers.as_ref();
        res.grade = Some(grade::grade(&Metrics {
            connect_ms: res.connect_ms,
            handshake_ms: res.handshake_ms,
            ping_ms: res.ping_ms.as_ref().and_then(|p| p.median),
            headers_served: headers.map_or(0, |h| h.served),
            headers_requested: headers.map_or(p.depth, |h| h.requested),
            headers_per_sec: headers.map_or(0.0, |h| h.per_sec),
            timeouts: headers.map_or(0, |h| h.stalls),
            validation_failures: headers.map_or(0, |h| u32::from(!h.valid)),
        }));
    }
    if !errors.is_empty() {
        res.error = Some(errors.join("; "));
    }
    res
}

async fn run_inner(
    t: &Target<'_>,
    reference: &CheckedReference,
    p: &Params,
    deadline: Instant,
    meter: &Meter,
    res: &mut BenchResult,
    errors: &mut Vec<String>,
) {
    let started = Instant::now();
    let connect_limit = t.timeouts.connect.min(deadline.saturating_duration_since(started));
    let stream = match transport::connect(t.route, t.addr, connect_limit).await {
        Ok(s) => s,
        Err(e) => return errors.push(format!("{e:#}")),
    };
    res.connect_ms = Some(ms(started.elapsed()));
    let connected = Instant::now();
    let mut s = Session::new(stream, t.net, meter, deadline);

    let version = match handshake(&mut s, t, deadline.min(connected + t.timeouts.handshake)).await {
        Ok(v) => v,
        Err(e) => return errors.push(format!("handshake: {e:#}")),
    };
    res.handshake_ms = Some(ms(connected.elapsed()));
    res.user_agent = Some(version.user_agent.clone());
    res.protocol = Some(version.protocol);
    res.height = Some(version.start_height);
    res.ok = true;

    let pings = ping(&mut s, p.pings, deadline.min(Instant::now() + PING_PHASE)).await;
    res.ping_ms = Some(ping_stats(&pings.samples));
    if let Some(e) = pings.error {
        return errors.push(format!("ping: {e:#}"));
    }

    let headers_deadline = deadline.checked_sub(MNLISTDIFF_RESERVE).unwrap_or(deadline).max(Instant::now());
    let (stats, error) = headers(&mut s, reference, p, version.start_height, headers_deadline).await;
    res.headers = Some(stats);
    if let Some(e) = error {
        errors.push(format!("headers: {e}"));
    }

    let protocol = version.protocol.min(PROTOCOL_VERSION);
    let (stats, error) = mnlistdiff(&mut s, reference, protocol, t.expected_mns, deadline).await;
    res.mnlistdiff = stats;
    if let Some(e) = error {
        errors.push(format!("mnlistdiff: {e}"));
    }
}

/// Whether `t` completes a version handshake within `limit` (nothing else is asked of it).
pub async fn handshakes(t: &Target<'_>, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    let Ok(stream) = transport::connect(t.route, t.addr, limit).await else { return false };
    let mut s = Session::new(stream, t.net, &Meter::default(), deadline);
    handshake(&mut s, t, deadline).await.is_ok()
}

/// version → (their version) → verack → (their verack). Returns once both have arrived.
async fn handshake(s: &mut Session, t: &Target<'_>, until: Instant) -> Result<p2p::Version> {
    s.send("version", &p2p::version_payload(t.addr, 0)).await?;
    let mut version = None;
    let mut verack = false;
    while version.is_none() || !verack {
        let (cmd, payload) = s.recv(until, None).await?;
        match cmd.as_str() {
            "version" if version.is_none() => {
                version = Some(p2p::parse_version(&payload)?);
                s.send("verack", &[]).await?;
            }
            "verack" => verack = true,
            _ => {}
        }
    }
    Ok(version.unwrap())
}

struct Pings {
    samples: Vec<f64>,
    /// Set when the connection broke (not when a pong was merely late).
    error: Option<anyhow::Error>,
}

/// Sends `count` pings one after another, timing each pong. A ping without a pong in time ends
/// the phase without failing the test.
async fn ping(s: &mut Session, count: usize, until: Instant) -> Pings {
    let mut samples = Vec::with_capacity(count);
    for _ in 0..count {
        let nonce: u64 = rand::random();
        let sent = Instant::now();
        if let Err(e) = s.send("ping", &nonce.to_le_bytes()).await {
            return Pings { samples, error: Some(e) };
        }
        let wait = until.min(sent + PING_TIMEOUT);
        loop {
            match s.recv(wait, None).await {
                Ok((cmd, payload)) if cmd == "pong" && wire::parse_nonce(&payload).ok() == Some(nonce) => {
                    samples.push(ms(sent.elapsed()));
                    break;
                }
                Ok(_) => {}
                Err(e) if is_timeout(&e) => return Pings { samples, error: None },
                Err(e) => return Pings { samples, error: Some(e) },
            }
        }
    }
    Pings { samples, error: None }
}

fn ping_stats(samples: &[f64]) -> PingStats {
    let mut v = samples.to_vec();
    v.sort_by(f64::total_cmp);
    let median = match v.len() {
        0 => None,
        n if n % 2 == 1 => Some(v[n / 2]),
        n => Some(round1((v[n / 2 - 1] + v[n / 2]) / 2.0)),
    };
    PingStats { min: v.first().copied(), median, max: v.last().copied(), samples: v.len() }
}

/// Downloads the headers after `tip - depth` up to the reference tip, checking that each links to
/// the one before and that the reference heights match. Like spv-network-health, a node is only
/// asked for chain it claims to have: up to its advertised height, if that's lower (0 or less means
/// it didn't say).
async fn headers(
    s: &mut Session,
    reference: &CheckedReference,
    p: &Params,
    node_height: i32,
    until: Instant,
) -> (HeaderStats, Option<String>) {
    let start = reference.tip.saturating_sub(p.depth);
    let end = if node_height > 0 { reference.tip.min((node_height as u32).max(start)) } else { reference.tip };
    // End on a reference height, so every header counted as served has been checked against one.
    let end = start + (end - start) / crate::reference::STEP * crate::reference::STEP;
    let mut stats = HeaderStats {
        start_height: start,
        requested: end - start,
        served: 0,
        valid: true,
        seconds: 0.0,
        per_sec: 0.0,
        bytes: 0,
        stalls: 0,
    };
    if end == start {
        return (
            stats,
            Some(format!("node is at height {node_height}; the test needs {}", start + crate::reference::STEP)),
        );
    }
    let Some(&start_hash) = reference.hashes.get(&start) else {
        return (stats, Some(format!("no reference hash at {start}")));
    };
    let (mut last, mut height) = (start_hash, start);
    // The highest height checked against a reference hash; only headers up to it count as served.
    let mut verified = start;
    // Time spent waiting on the node only: our own hashing between batches doesn't count.
    let mut waiting = Duration::ZERO;
    // Locators already answered, to recognize late duplicate answers after a re-request.
    let mut answered = HashSet::new();
    let progress = |h: u32| format!("{}/{}", h - start, end - start);
    let error = 'download: loop {
        if height >= end {
            break None;
        }
        let sent = Instant::now();
        let reply = match s.request("getheaders", &wire::getheaders_payload(PROTOCOL_VERSION, &last)).await {
            Ok(()) => next_headers(s, &last, &answered, until, p.stall, &mut stats.bytes).await,
            Err(e) => Err(e),
        };
        waiting += sent.elapsed();
        let batch = match reply {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => {
                stats.valid = false;
                break Some(format!("bad headers message: {e:#}"));
            }
            Err(e) if is_timeout(&e) => {
                stats.stalls += 1;
                if Instant::now() >= until {
                    break Some(format!("out of time at {}", progress(height)));
                }
                if stats.stalls > MAX_STALLS {
                    break Some(format!("stalled at {}", progress(height)));
                }
                continue;
            }
            Err(e) => {
                stats.stalls += 1;
                break Some(format!("{e:#}"));
            }
        };
        answered.insert(last);
        let Some(first) = batch.first() else {
            break Some(format!("node ran out of headers at {}", progress(height)));
        };
        if wire::prev_hash(first) != last {
            stats.valid = false;
            break Some(format!("header at {} doesn't connect", height + 1));
        }
        let (batch, hashes) = match hash_all(batch).await {
            Ok(v) => v,
            Err(e) => break Some(e),
        };
        for (h, hash) in batch.iter().zip(hashes) {
            if wire::prev_hash(h) != last {
                stats.valid = false;
                break 'download Some(format!("header at {} doesn't connect", height + 1));
            }
            last = hash;
            height += 1;
            if let Some(r) = reference.hashes.get(&height) {
                if *r != last {
                    stats.valid = false;
                    break 'download Some(format!("header at {height} doesn't match ours"));
                }
                verified = height;
            }
            if height == end {
                break;
            }
        }
    };
    stats.served = verified - start;
    stats.seconds = round3(waiting.as_secs_f64());
    if stats.seconds > 0.0 {
        stats.per_sec = round1(f64::from(stats.served) / waiting.as_secs_f64());
    }
    (stats, error)
}

/// The next `headers` reply to a request from `from`, skipping late duplicate answers to requests
/// already answered (sent again after a stall). The outer error is the connection's (or a stall),
/// the inner one a malformed message.
async fn next_headers(
    s: &mut Session,
    from: &Hash,
    answered: &HashSet<Hash>,
    until: Instant,
    stall: Duration,
    bytes: &mut u64,
) -> Result<Result<Vec<RawHeader>>> {
    loop {
        let (cmd, payload) = s.recv(until, Some(stall)).await?;
        if cmd != "headers" {
            continue;
        }
        *bytes += 24 + payload.len() as u64;
        let batch = wire::parse_headers(&payload);
        if let Ok(b) = &batch
            && let Some(prev) = b.first().map(wire::prev_hash)
            && prev != *from
            && answered.contains(&prev)
        {
            continue;
        }
        return Ok(batch);
    }
}

/// X11 costs ~20 µs a header: hash off the async workers, so concurrent tests' timings (and a
/// one-core vantage's runtime) aren't held up by it.
async fn hash_all(batch: Vec<RawHeader>) -> Result<(Vec<RawHeader>, Vec<Hash>), String> {
    tokio::task::spawn_blocking(move || {
        let hashes = batch.iter().map(wire::block_hash).collect();
        (batch, hashes)
    })
    .await
    .map_err(|e| format!("hashing: {e}"))
}

/// Asks for the whole masternode list (diff from the empty list at genesis, base hash all zeros)
/// at the reference tip, and checks it parses, is for that block, and is about the right size.
async fn mnlistdiff(
    s: &mut Session,
    reference: &CheckedReference,
    protocol: i32,
    expected: Option<usize>,
    deadline: Instant,
) -> (Option<MnListDiffStats>, Option<String>) {
    let sent = Instant::now();
    if deadline.saturating_duration_since(sent) < Duration::from_secs(5) {
        return (None, Some("no time left".into()));
    }
    if protocol < wire::MIN_MNLISTDIFF_PROTOCOL {
        return (None, Some(format!("protocol {protocol} not supported")));
    }
    let mut stats = MnListDiffStats::default();
    let reply = match s.send("getmnlistd", &wire::getmnlistd_payload(&[0; 32], &reference.tip_hash)).await {
        Ok(()) => loop {
            match s.recv(deadline.min(sent + MNLISTDIFF_TIMEOUT), None).await {
                Ok((cmd, payload)) if cmd == "mnlistdiff" => break Ok(payload),
                Ok(_) => {}
                Err(e) => break Err(e),
            }
        },
        Err(e) => Err(e),
    };
    stats.seconds = round3(sent.elapsed().as_secs_f64());
    let payload = match reply {
        Ok(p) => p,
        Err(e) => return (Some(stats), Some(format!("{e:#}"))),
    };
    stats.bytes = 24 + payload.len() as u64;
    let diff = match wire::parse_mnlistdiff(&payload, protocol) {
        Ok(d) => d,
        Err(e) => return (Some(stats), Some(format!("unparseable: {e:#}"))),
    };
    stats.entries = Some(diff.entries);
    let problem = if diff.block_hash != reference.tip_hash {
        Some("for another block".to_string())
    } else if diff.deleted != 0 {
        Some(format!("{} deletions in a diff from the empty list", diff.deleted))
    } else {
        expected
            .filter(|&n| !plausible(diff.entries, n))
            .map(|n| format!("{} entries, expected about {n}", diff.entries))
    };
    stats.ok = problem.is_none();
    (Some(stats), problem)
}

fn plausible(entries: usize, expected: usize) -> bool {
    (entries as f64 - expected as f64).abs() <= (expected as f64 * MNLIST_TOLERANCE).max(10.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_stats_median() {
        assert_eq!(ping_stats(&[]), PingStats { min: None, median: None, max: None, samples: 0 });
        let s = ping_stats(&[30.0, 10.0, 20.0, 500.0, 25.0]);
        assert_eq!((s.min, s.median, s.max, s.samples), (Some(10.0), Some(25.0), Some(500.0), 5));
        assert_eq!(ping_stats(&[10.0, 20.0]).median, Some(15.0));
    }

    #[test]
    fn mn_count_plausibility() {
        assert!(plausible(3000, 3005));
        assert!(plausible(2750, 3005));
        assert!(!plausible(2000, 3005));
        // Small lists get an absolute tolerance.
        assert!(plausible(15, 20));
        assert!(!plausible(0, 399));
    }

    #[test]
    fn reference_checks() {
        let h = "00000000000000000000000000000000000000000000000000000000000000ab";
        let mut r = Reference { tip: 10, tip_hash: h.into(), hashes: [(10, h.into())].into() };
        let c = CheckedReference::new(&r).unwrap();
        assert_eq!(c.tip_hash[0], 0xab);
        assert_eq!(c.hashes[&10], c.tip_hash);
        r.hashes.insert(4, "zz".into());
        assert!(CheckedReference::new(&r).is_err());
        r.hashes.clear();
        assert!(CheckedReference::new(&r).unwrap_err().to_string().contains("tip"));
        // Heights arrive as JSON object keys.
        let r: Reference =
            serde_json::from_str(&format!(r#"{{"tip":10,"tip_hash":"{h}","hashes":{{"10":"{h}"}}}}"#)).unwrap();
        assert_eq!(r.hashes[&10], h);
        assert!(serde_json::from_str::<Reference>(r#"{"tip":1,"tip_hash":"","hashes":{"x":""}}"#).is_err());
    }

    /// A synthetic chain: each header's prev is the X11 hash of the one before.
    fn chain(len: usize, salt: u8) -> (Vec<wire::RawHeader>, Vec<Hash>) {
        let mut headers = Vec::with_capacity(len);
        let mut hashes = Vec::with_capacity(len);
        let mut prev = [0u8; 32];
        for i in 0..len {
            let mut h = [salt; 80];
            h[4..36].copy_from_slice(&prev);
            h[76..80].copy_from_slice(&(i as u32).to_le_bytes());
            prev = wire::block_hash(&h);
            headers.push(h);
            hashes.push(prev);
        }
        (headers, hashes)
    }

    #[derive(Clone, Copy)]
    struct Behavior {
        /// Leave the first getheaders unanswered (a stall), answering only the re-request.
        stall_once: bool,
        /// Ignore our pings.
        no_pong: bool,
        /// Pause in the middle of the first headers message, longer than the stall budget.
        split_first: bool,
    }

    /// A node that sends the usual unsolicited messages and pings us, then serves `served`.
    async fn fake_node(listener: tokio::net::TcpListener, served: Vec<wire::RawHeader>, b: Behavior) -> bool {
        let magic = Network::Mainnet.magic();
        let (mut stream, _) = listener.accept().await.unwrap();
        let hashes: Vec<Hash> = served.iter().map(wire::block_hash).collect();
        let me: PeerAddr = "127.0.0.1:1".parse().unwrap();
        let mut got_pong = false;
        let mut stalled = false;
        let send = |cmd: &str, p: &[u8]| p2p::encode(magic, cmd, p);
        while let Ok((cmd, payload)) = p2p::read_message(&mut stream, magic, MAX_MESSAGE).await {
            let reply: Vec<Vec<u8>> = match cmd.as_str() {
                "version" => vec![
                    send("version", &p2p::version_payload(&me, served.len() as i32 - 1)),
                    send("sendheaders", &[]),
                    send("verack", &[]),
                    send("sendcmpct", &[0; 9]),
                    send("ping", &42u64.to_le_bytes()),
                    send("inv", &[0]),
                ],
                "pong" => {
                    got_pong |= wire::parse_nonce(&payload).unwrap() == 42;
                    vec![]
                }
                "ping" if !b.no_pong => vec![send("pong", &payload)],
                "getheaders" => {
                    let from: Hash = payload[5..37].try_into().unwrap();
                    let at = hashes.iter().position(|h| *h == from).map_or(0, |i| i + 1);
                    let mut p = Vec::new();
                    let batch = &served[at..(at + wire::MAX_HEADERS).min(served.len())];
                    p2p::write_compact_size(&mut p, batch.len() as u64);
                    for h in batch {
                        p.extend_from_slice(h);
                        p.push(0);
                    }
                    if b.split_first && !stalled {
                        stalled = true;
                        let msg = send("headers", &p);
                        stream.write_all(&msg[..msg.len() / 2]).await.unwrap();
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                        vec![msg[msg.len() / 2..].to_vec()]
                    } else if b.stall_once && !stalled {
                        // Answer late: after the bench's stall budget, so it has re-requested by then.
                        stalled = true;
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                        vec![send("addr", &[0]), send("headers", &p)]
                    } else {
                        vec![send("headers", &p)]
                    }
                }
                "getmnlistd" => vec![send("mnlistdiff", &[0; 10])],
                _ => vec![],
            };
            for m in reply {
                stream.write_all(&m).await.unwrap();
            }
        }
        got_pong
    }

    async fn bench_against(
        served: Vec<wire::RawHeader>,
        reference: &CheckedReference,
        b: Behavior,
    ) -> (BenchResult, bool) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: PeerAddr = listener.local_addr().unwrap().to_string().parse().unwrap();
        let node = tokio::spawn(fake_node(listener, served, b));
        let timeouts =
            Timeouts { connect: Duration::from_secs(2), handshake: Duration::from_secs(2), getaddr: Duration::ZERO };
        let target = Target {
            net: Network::Mainnet,
            addr: &addr,
            route: &Route::Direct,
            timeouts: &timeouts,
            expected_mns: None,
        };
        let p = Params { depth: 4000, stall: Duration::from_secs(1), limit: Duration::from_secs(30), pings: 3 };
        let r = run(&target, reference, &p, "test", &Meter::default()).await;
        (r, node.await.unwrap())
    }

    fn reference_for(chain: &[Hash], depth: usize) -> CheckedReference {
        let tip = chain.len() - 1;
        let hashes = (0..=depth / 2000).map(|k| tip - depth + k * 2000).map(|h| (h as u32, chain[h])).collect();
        CheckedReference { tip: tip as u32, tip_hash: chain[tip], hashes }
    }

    #[tokio::test]
    async fn end_to_end_against_a_fake_node() {
        let (headers, hashes) = chain(5001, 1);
        let reference = reference_for(&hashes, 4000);
        let (r, got_pong) = bench_against(
            headers.clone(),
            &reference,
            Behavior { stall_once: false, no_pong: false, split_first: false },
        )
        .await;
        assert!(got_pong, "the bench must answer the node's ping");
        assert!(r.ok);
        assert_eq!(r.height, Some(5000));
        let h = r.headers.unwrap();
        assert_eq!((h.start_height, h.requested, h.served, h.valid, h.stalls), (1000, 4000, 4000, true, 0));
        assert_eq!(h.bytes, 2 * (24 + 3 + 2000 * 81));
        assert_eq!(r.ping_ms.unwrap().samples, 3);
        // The fake mnlistdiff is junk: reported, not graded.
        let m = r.mnlistdiff.unwrap();
        assert_eq!((m.ok, m.entries, m.bytes), (false, None, 34));
        assert!(r.error.unwrap().starts_with("mnlistdiff: unparseable"));
        let g = r.grade.unwrap();
        assert_eq!((g.completeness, g.reliability), (100.0, 100.0));

        // A stall: re-requested, the late duplicate answer is ignored, and the stall costs reliability.
        let (r, _) = bench_against(
            headers.clone(),
            &reference,
            Behavior { stall_once: true, no_pong: true, split_first: false },
        )
        .await;
        let h = r.headers.unwrap();
        assert_eq!((h.served, h.valid, h.stalls), (4000, true, 1));
        assert_eq!(r.ping_ms.unwrap().samples, 0);
        assert_eq!(r.grade.unwrap().reliability, 75.0);

        // Quiet in the middle of a message: a stall, but the message isn't lost or corrupted.
        let b = Behavior { stall_once: false, no_pong: false, split_first: true };
        let (r, _) = bench_against(headers.clone(), &reference, b).await;
        let h = r.headers.unwrap();
        assert_eq!((h.served, h.valid, h.stalls), (4000, true, 1), "{:?}", r.error);
        assert!(r.mnlistdiff.unwrap().bytes > 0, "the connection is still usable");

        // A chain that matches up to 3000 but not at the tip: only the verified part counts.
        let mut wrong_tip = reference.clone();
        wrong_tip.hashes.insert(5000, [0xee; 32]);
        let (r, _) = bench_against(
            headers.clone(),
            &wrong_tip,
            Behavior { stall_once: false, no_pong: false, split_first: false },
        )
        .await;
        let h = r.headers.unwrap();
        assert_eq!((h.served, h.valid), (2000, false));
        assert!(r.error.unwrap().contains("header at 5000 doesn't match ours"));
        assert_ne!(r.grade.unwrap().letter, "A");

        // Another chain from the start height on: the first batch doesn't connect.
        let (other, _) = chain(5001, 2);
        let mut forked = headers[..=1000].to_vec();
        forked.extend_from_slice(&other[1001..]);
        let (r, _) =
            bench_against(forked, &reference, Behavior { stall_once: false, no_pong: false, split_first: false }).await;
        let h = r.headers.unwrap();
        assert_eq!((h.served, h.valid), (0, false));
        assert!(r.error.unwrap().contains("doesn't connect"));
        assert_eq!(r.grade.unwrap().letter, "F");
    }
}

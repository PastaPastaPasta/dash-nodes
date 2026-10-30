//! Evonode Platform health: every PLATFORM_INTERVAL, each registered evonode's Platform endpoints are
//! checked from here.
//!
//! - Platform P2P (Tenderdash): a TCP connection to its registered `platform_p2p` address.
//! - Platform HTTPS (DAPI): a gRPC-web `getStatus` call over TLS to `platform_https`, verifying the
//!   certificate like any client would. The answer carries the DAPI, Drive and Tenderdash versions,
//!   Drive's protocol versions, the Platform chain height, and the ProTx hash the node claims, which
//!   must be this evonode's. The certificate's issuer and expiry come from the TLS session.
//!
//! The latest result per evonode is kept in `platform_status`, and daily counts in `platform_daily`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use futures::StreamExt;
use rusqlite::params;
use serde_json::{Value, json};
use tokio::net::TcpStream;

use crate::db;
use crate::state::NetCtx;

const PLATFORM_INTERVAL: Duration = Duration::from_secs(15 * 60);
const CONCURRENCY: usize = 32;
const TIMEOUT: Duration = Duration::from_secs(10);

/// What one check found. Errors say which step failed.
#[derive(Debug, Default, PartialEq)]
pub struct Check {
    pub p2p_ms: Option<i64>,
    pub p2p_error: Option<String>,
    pub https_ms: Option<i64>,
    pub https_error: Option<String>,
    pub cert_issuer: Option<String>,
    pub cert_expires: Option<i64>,
    pub status: Option<Status>,
}

/// The parts of `getStatus` worth showing.
#[derive(Debug, Default, PartialEq)]
pub struct Status {
    pub dapi: Option<String>,
    pub drive: Option<String>,
    pub tenderdash: Option<String>,
    pub drive_protocol_latest: Option<u64>,
    pub drive_protocol_current: Option<u64>,
    pub height: Option<u64>,
    pub catching_up: bool,
    pub peers: Option<u64>,
    pub pro_tx_hash: Option<String>,
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(TIMEOUT)
        .connect_timeout(TIMEOUT)
        .tls_info(true)
        .user_agent(crate::p2p::user_agent())
        .build()
        .expect("http client")
}

/// One check, for the `platform-check` command.
pub async fn check_once(p2p: &str, https: &str) -> Check {
    check(&client(), Some(p2p), Some(https)).await
}

pub async fn run(ctx: Arc<NetCtx>) {
    let client = client();
    loop {
        if ctx.mn_loaded.load(std::sync::atomic::Ordering::Acquire)
            && let Err(e) = round(&ctx, &client).await
        {
            tracing::warn!(net = ctx.net.name(), "platform checks: {e:#}");
        }
        tokio::time::sleep(PLATFORM_INTERVAL).await;
    }
}

async fn round(ctx: &NetCtx, client: &reqwest::Client) -> Result<()> {
    let evonodes: Vec<(String, Option<String>, Option<String>)> = ctx
        .db
        .read(|c| {
            Ok(db::query_all(
                c,
                "SELECT protx, platform_p2p, platform_https FROM masternodes WHERE type = 'Evo' AND removed IS NULL",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?)
        })
        .await?;
    let started = Instant::now();
    let results: Vec<(String, Check)> = futures::stream::iter(evonodes)
        .map(|(protx, p2p, https)| async move {
            let c = check(client, p2p.as_deref(), https.as_deref()).await;
            (protx, c)
        })
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;
    let n = results.len();
    let ok = results.iter().filter(|(p, c)| healthy(p, c)).count();
    ctx.db.write(move |c| store(c, &results)).await?;
    tracing::info!(
        net = ctx.net.name(),
        "platform checks: {ok} of {n} evonodes healthy, in {:.0} s",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Both endpoints answered, and the HTTPS one as this evonode.
pub fn healthy(protx: &str, c: &Check) -> bool {
    c.p2p_error.is_none()
        && c.https_error.is_none()
        && c.status.as_ref().is_some_and(|s| s.pro_tx_hash.as_deref().is_none_or(|p| p == protx))
}

fn store(c: &mut rusqlite::Connection, results: &[(String, Check)]) -> Result<()> {
    let now = db::now();
    let day = db::day_of(now);
    let tx = c.transaction()?;
    {
        let mut latest = tx.prepare_cached(
            "INSERT OR REPLACE INTO platform_status (protx, ts, p2p_ms, p2p_error, https_ms, https_error, cert_issuer,
                cert_expires, dapi, drive, tenderdash, drive_protocol_latest, drive_protocol_current, height,
                catching_up, peers, node_protx)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
        )?;
        let mut daily = tx.prepare_cached(
            "INSERT INTO platform_daily (protx, day, attempts, ok) VALUES (?1, ?2, 1, ?3)
             ON CONFLICT(protx, day) DO UPDATE SET attempts = attempts + 1, ok = ok + excluded.ok",
        )?;
        for (protx, r) in results {
            let s = r.status.as_ref();
            latest.execute(params![
                protx,
                now,
                r.p2p_ms,
                r.p2p_error,
                r.https_ms,
                r.https_error,
                r.cert_issuer,
                r.cert_expires,
                s.and_then(|s| s.dapi.clone()),
                s.and_then(|s| s.drive.clone()),
                s.and_then(|s| s.tenderdash.clone()),
                s.and_then(|s| s.drive_protocol_latest.map(|v| v as i64)),
                s.and_then(|s| s.drive_protocol_current.map(|v| v as i64)),
                s.and_then(|s| s.height.map(|v| v as i64)),
                s.map(|s| s.catching_up),
                s.and_then(|s| s.peers.map(|v| v as i64)),
                s.and_then(|s| s.pro_tx_hash.clone()),
            ])?;
            daily.execute(params![protx, day, i64::from(healthy(protx, r))])?;
        }
    }
    tx.commit()?;
    Ok(())
}

async fn check(client: &reqwest::Client, p2p: Option<&str>, https: Option<&str>) -> Check {
    let mut c = Check::default();
    match p2p {
        Some(addr) => {
            let t = Instant::now();
            match tokio::time::timeout(TIMEOUT, TcpStream::connect(addr)).await {
                Ok(Ok(_)) => c.p2p_ms = Some(t.elapsed().as_millis() as i64),
                Ok(Err(e)) => c.p2p_error = Some(format!("connect: {}", e.kind())),
                Err(_) => c.p2p_error = Some("connect: timed out".into()),
            }
        }
        None => c.p2p_error = Some("no Platform P2P address registered".into()),
    }
    match https {
        Some(addr) => {
            let t = Instant::now();
            match get_status(client, addr).await {
                Ok((status, cert)) => {
                    c.https_ms = Some(t.elapsed().as_millis() as i64);
                    c.status = Some(status);
                    if let Some(der) = cert {
                        (c.cert_issuer, c.cert_expires) = cert_facts(&der);
                    }
                }
                Err(e) => c.https_error = Some(format!("{e:#}")),
            }
        }
        None => c.https_error = Some("no Platform HTTPS address registered".into()),
    }
    c
}

/// `getStatus` over gRPC-web. Returns the decoded status and the server's leaf certificate (DER).
async fn get_status(client: &reqwest::Client, addr: &str) -> Result<(Status, Option<Vec<u8>>)> {
    // GetStatusRequest { v0: {} }, in one gRPC-web data frame.
    let body: &[u8] = &[0, 0, 0, 0, 2, 0x0a, 0x00];
    let resp = client
        .post(format!("https://{addr}/org.dash.platform.dapi.v0.Platform/getStatus"))
        .header("content-type", "application/grpc-web+proto")
        .header("x-grpc-web", "1")
        .body(body)
        .send()
        .await
        .map_err(|e| anyhow!(describe(&e)))?;
    let cert = resp.extensions().get::<reqwest::tls::TlsInfo>().and_then(|t| t.peer_certificate().map(<[u8]>::to_vec));
    if !resp.status().is_success() {
        bail!("HTTP {}", resp.status());
    }
    let grpc_message = resp.headers().get("grpc-message").and_then(|v| v.to_str().ok()).map(str::to_owned);
    let bytes = resp.bytes().await.map_err(|e| anyhow!(describe(&e)))?;
    let msg = first_data_frame(&bytes)
        .ok_or_else(|| anyhow!("no answer ({})", grpc_message.as_deref().unwrap_or("empty")))?;
    Ok((parse_status(msg)?, cert))
}

/// A reqwest error as one line, with the TLS or connection reason it wraps.
fn describe(e: &reqwest::Error) -> String {
    let mut s = if e.is_timeout() {
        "timed out".to_owned()
    } else if e.is_connect() {
        "connect".to_owned()
    } else {
        "request".to_owned()
    };
    let mut src = std::error::Error::source(e);
    let mut last = None;
    while let Some(inner) = src {
        last = Some(inner.to_string());
        src = inner.source();
    }
    if let Some(l) = last {
        s = format!("{s}: {l}");
    }
    s
}

/// The payload of the first gRPC-web data frame (flag 0x00): frames are a flag byte, a big-endian
/// length and the message; trailers come in a frame flagged 0x80.
fn first_data_frame(buf: &[u8]) -> Option<&[u8]> {
    let mut rest = buf;
    while rest.len() >= 5 {
        let len = u32::from_be_bytes(rest[1..5].try_into().ok()?) as usize;
        let body = rest.get(5..5 + len)?;
        if rest[0] & 0x80 == 0 {
            return Some(body);
        }
        rest = &rest[5 + len..];
    }
    None
}

/// One protobuf field: its number and either a varint or length-delimited bytes.
enum Field<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

fn varint(buf: &mut &[u8]) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let (&b, rest) = buf.split_first()?;
        *buf = rest;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// The fields of a protobuf message, in order (fixed-width fields are skipped).
fn fields(mut buf: &[u8]) -> Result<Vec<(u64, Field<'_>)>> {
    let mut out = Vec::new();
    while !buf.is_empty() {
        let key = varint(&mut buf).ok_or_else(|| anyhow!("bad protobuf"))?;
        let (num, wire) = (key >> 3, key & 7);
        match wire {
            0 => out.push((num, Field::Varint(varint(&mut buf).ok_or_else(|| anyhow!("bad protobuf"))?))),
            2 => {
                let len = varint(&mut buf).ok_or_else(|| anyhow!("bad protobuf"))? as usize;
                let b = buf.get(..len).ok_or_else(|| anyhow!("bad protobuf"))?;
                out.push((num, Field::Bytes(b)));
                buf = &buf[len..];
            }
            1 => buf = buf.get(8..).ok_or_else(|| anyhow!("bad protobuf"))?,
            5 => buf = buf.get(4..).ok_or_else(|| anyhow!("bad protobuf"))?,
            _ => bail!("bad protobuf"),
        }
    }
    Ok(out)
}

fn msg(buf: &[u8], num: u64) -> Option<&[u8]> {
    fields(buf).ok()?.into_iter().find_map(|(n, f)| match f {
        Field::Bytes(b) if n == num => Some(b),
        _ => None,
    })
}

fn uint(buf: &[u8], num: u64) -> Option<u64> {
    fields(buf).ok()?.into_iter().find_map(|(n, f)| match f {
        Field::Varint(v) if n == num => Some(v),
        _ => None,
    })
}

fn text(buf: &[u8], num: u64) -> Option<String> {
    msg(buf, num).and_then(|b| std::str::from_utf8(b).ok()).filter(|s| !s.is_empty()).map(str::to_owned)
}

/// GetStatusResponse (dapi-grpc platform.proto): v0 = 1 { version = 1 { software = 1 { dapi = 1,
/// drive = 2, tenderdash = 3 }, protocol = 2 { drive = 2 { latest = 3, current = 4 } } }, node = 2
/// { pro_tx_hash = 2 }, chain = 3 { catching_up = 1, latest_block_height = 4 }, network = 4
/// { peers_count = 2 } }.
fn parse_status(resp: &[u8]) -> Result<Status> {
    let v0 = msg(resp, 1).ok_or_else(|| anyhow!("getStatus: no v0 answer"))?;
    let version = msg(v0, 1).unwrap_or_default();
    let software = msg(version, 1).unwrap_or_default();
    let drive_protocol = msg(version, 2).and_then(|p| msg(p, 2)).unwrap_or_default();
    let chain = msg(v0, 3).unwrap_or_default();
    Ok(Status {
        dapi: text(software, 1),
        drive: text(software, 2),
        tenderdash: text(software, 3),
        drive_protocol_latest: uint(drive_protocol, 3),
        drive_protocol_current: uint(drive_protocol, 4),
        height: uint(chain, 4),
        catching_up: uint(chain, 1) == Some(1),
        peers: msg(v0, 4).and_then(|n| uint(n, 2)),
        pro_tx_hash: msg(v0, 2).and_then(|n| msg(n, 2)).map(|b| b.iter().map(|x| format!("{x:02x}")).collect()),
    })
}

/// The issuer's organization (or common name) and the expiry (unix time) of a DER certificate.
fn cert_facts(der: &[u8]) -> (Option<String>, Option<i64>) {
    let Ok((_, cert)) = x509_parser::parse_x509_certificate(der) else { return (None, None) };
    let issuer = cert
        .issuer()
        .iter_organization()
        .chain(cert.issuer().iter_common_name())
        .find_map(|a| a.as_str().ok().map(str::to_owned));
    (issuer, Some(cert.validity().not_after.timestamp()))
}

/// The latest check of one evonode, for its page.
pub fn status_json(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    Ok(json!({
        "ts": r.get::<_, i64>("ts")?,
        "p2p": {"ms": r.get::<_, Option<i64>>("p2p_ms")?, "error": r.get::<_, Option<String>>("p2p_error")?},
        "https": {"ms": r.get::<_, Option<i64>>("https_ms")?, "error": r.get::<_, Option<String>>("https_error")?},
        "cert": {"issuer": r.get::<_, Option<String>>("cert_issuer")?, "expires": r.get::<_, Option<i64>>("cert_expires")?},
        "software": {
            "dapi": r.get::<_, Option<String>>("dapi")?, "drive": r.get::<_, Option<String>>("drive")?,
            "tenderdash": r.get::<_, Option<String>>("tenderdash")?,
        },
        "drive_protocol": {"latest": r.get::<_, Option<i64>>("drive_protocol_latest")?, "current": r.get::<_, Option<i64>>("drive_protocol_current")?},
        "height": r.get::<_, Option<i64>>("height")?,
        "catching_up": r.get::<_, Option<bool>>("catching_up")?,
        "peers": r.get::<_, Option<i64>>("peers")?,
        "node_protx": r.get::<_, Option<String>>("node_protx")?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A framed answer shaped like a real one (values from an evonode on mainnet, 2026-10-30).
    fn real_answer() -> Vec<u8> {
        let mut software = vec![0x0a, 5];
        software.extend(b"5.0.1");
        software.extend([0x12, 5]);
        software.extend(b"5.0.1");
        software.extend([0x1a, 5]);
        software.extend(b"1.8.2");
        let drive = [0x18, 0x0e, 0x20, 0x0d, 0x28, 0x0e]; // latest 14, current 13, next 14
        let mut protocol = vec![0x12, drive.len() as u8];
        protocol.extend(drive);
        let mut version = vec![0x0a, software.len() as u8];
        version.extend(&software);
        version.extend([0x12, protocol.len() as u8]);
        version.extend(&protocol);
        let mut node = vec![0x12, 32];
        node.extend([0xee; 32]);
        let chain = [0x08, 0x00, 0x20, 0xb4, 0x8e, 0x0c]; // catching_up false, height 198452
        let network = [0x10, 0x2a]; // 42 peers
        let mut v0 = vec![0x0a, version.len() as u8];
        v0.extend(&version);
        v0.extend([0x12, node.len() as u8]);
        v0.extend(&node);
        v0.extend([0x1a, chain.len() as u8]);
        v0.extend(chain);
        v0.extend([0x22, network.len() as u8]);
        v0.extend(network);
        let mut resp = vec![0x0a, v0.len() as u8];
        resp.extend(&v0);
        // A data frame, then a trailer frame.
        let mut framed = vec![0, 0, 0, 0, resp.len() as u8];
        framed.extend(&resp);
        framed.extend([0x80, 0, 0, 0, 15]);
        framed.extend(b"grpc-status:0\r\n");
        framed
    }

    #[test]
    fn decodes_get_status() {
        let framed = real_answer();
        let s = parse_status(first_data_frame(&framed).unwrap()).unwrap();
        assert_eq!(
            s,
            Status {
                dapi: Some("5.0.1".into()),
                drive: Some("5.0.1".into()),
                tenderdash: Some("1.8.2".into()),
                drive_protocol_latest: Some(14),
                drive_protocol_current: Some(13),
                height: Some(198_452),
                catching_up: false,
                peers: Some(42),
                pro_tx_hash: Some("ee".repeat(32)),
            }
        );
        // Only trailers (an error answer): no data frame.
        assert!(first_data_frame(&[0x80, 0, 0, 0, 2, b'a', b'b']).is_none());
        assert!(parse_status(&[0x0a]).is_err());
    }

    #[test]
    fn health_needs_both_endpoints_and_the_right_protx() {
        let good = Check {
            status: Some(Status { pro_tx_hash: Some("aa".into()), ..Default::default() }),
            ..Default::default()
        };
        assert!(healthy("aa", &good));
        assert!(!healthy("bb", &good));
        let p2p_down = Check { p2p_error: Some("connect: timed out".into()), ..good };
        assert!(!healthy("aa", &p2p_down));
    }
}

//! Just enough of the Dash P2P protocol (v1 transport) to handshake with a node, read its
//! `version` message and optionally ask it for addresses.
//!
//! We connect as an ordinary, non-masternode, non-relaying peer, so the remote treats us like any
//! inbound light client: it answers our `version`, and answers one `getaddr` per connection.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::network::Network;

/// Protocol version we announce. Matches Dash Core v24; peers settle on min(ours, theirs).
pub const PROTOCOL_VERSION: i32 = 70242;
/// Largest message we accept. A full 1000-entry `addr` is ~30 KB; anything bigger isn't for us.
const MAX_PAYLOAD: usize = 64 * 1024;
/// Messages read while waiting for the getaddr reply before giving up on it.
const MAX_GETADDR_MESSAGES: usize = 64;
const MAX_SUBVERSION_LENGTH: usize = 256;
const MAX_ADDRS: u64 = 1000;

pub fn user_agent() -> String {
    format!("/dash-nodes:{}(nodes.dashhq.org)/", env!("CARGO_PKG_VERSION"))
}

#[derive(Clone, Debug)]
pub struct Timeouts {
    pub connect: Duration,
    /// From connect until we have the peer's `version`.
    pub handshake: Duration,
    /// How long to wait for an `addr` reply after `getaddr`.
    pub getaddr: Duration,
}

#[derive(Clone, Debug)]
pub struct Version {
    pub protocol: i32,
    pub services: u64,
    pub user_agent: String,
    pub start_height: i32,
    pub latency_ms: u32,
}

#[derive(Debug)]
pub struct Probe {
    pub version: Version,
    pub addrs: Vec<SocketAddr>,
}

/// Connects, exchanges versions and, if `getaddr` is set, collects the peer's address sample
/// (at most 1000 addresses). The whole exchange, writes included, is bounded by the timeouts.
pub async fn probe(net: Network, addr: SocketAddr, start_height: i32, getaddr: bool, t: &Timeouts) -> Result<Probe> {
    let budget = t.connect + t.handshake + if getaddr { t.getaddr } else { Duration::ZERO } + Duration::from_secs(2);
    timeout(budget, probe_inner(net, addr, start_height, getaddr, t)).await.map_err(|_| anyhow!("timeout"))?
}

async fn probe_inner(net: Network, addr: SocketAddr, start_height: i32, getaddr: bool, t: &Timeouts) -> Result<Probe> {
    let started = Instant::now();
    let mut stream = timeout(t.connect, TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow!("connect timeout"))?
        .map_err(|e| anyhow!("connect: {}", e.kind()))?;
    stream.set_nodelay(true).ok();

    let magic = net.magic();
    stream.write_all(&encode(magic, "version", &version_payload(addr, start_height))).await?;

    let deadline = started + t.handshake;
    let version = loop {
        let (cmd, payload) = read_before(&mut stream, magic, deadline).await?;
        if cmd == "version" {
            let mut v = parse_version(&payload)?;
            v.latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
            break v;
        }
    };
    stream.write_all(&encode(magic, "verack", &[])).await?;

    let mut addrs = Vec::new();
    if getaddr {
        // The peer only processes getaddr after the handshake; wait for its verack first.
        let deadline = Instant::now() + t.getaddr;
        let result: Result<()> = async {
            loop {
                let (cmd, _) = read_before(&mut stream, magic, deadline).await?;
                if cmd == "verack" {
                    break;
                }
            }
            stream.write_all(&encode(magic, "getaddr", &[])).await?;
            // We never answer pings: we're gone long before the peer would time us out.
            for _ in 0..MAX_GETADDR_MESSAGES {
                let (cmd, payload) = read_before(&mut stream, magic, deadline).await?;
                if cmd == "addr" {
                    let batch = parse_addr(&payload)?;
                    // A lone address is ordinary relay; the getaddr reply is the big batch.
                    let done = batch.len() > 1;
                    let room = MAX_ADDRS as usize - addrs.len();
                    addrs.extend(batch.into_iter().take(room));
                    if done || addrs.len() >= MAX_ADDRS as usize {
                        return Ok(());
                    }
                }
            }
            Ok(())
        }
        .await;
        // The version is what we came for; a missing addr reply isn't a failure.
        if let Err(e) = result {
            tracing::trace!(%addr, "getaddr: {e:#}");
        }
    }
    Ok(Probe { version, addrs })
}

async fn read_before(stream: &mut TcpStream, magic: [u8; 4], deadline: Instant) -> Result<(String, Vec<u8>)> {
    let left = deadline.saturating_duration_since(Instant::now());
    timeout(left, read_message(stream, magic)).await.map_err(|_| anyhow!("timeout"))?
}

async fn read_message(stream: &mut TcpStream, magic: [u8; 4]) -> Result<(String, Vec<u8>)> {
    let mut header = [0u8; 24];
    stream.read_exact(&mut header).await.map_err(|e| anyhow!("read: {}", e.kind()))?;
    if header[..4] != magic {
        bail!("bad magic {:02x?}", &header[..4]);
    }
    let cmd_bytes = &header[4..16];
    let end = cmd_bytes.iter().position(|&b| b == 0).unwrap_or(12);
    let cmd = std::str::from_utf8(&cmd_bytes[..end]).context("command not ascii")?.to_string();
    let len = u32::from_le_bytes(header[16..20].try_into().unwrap()) as usize;
    if len > MAX_PAYLOAD {
        bail!("{cmd} payload too large ({len})");
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await.map_err(|e| anyhow!("read: {}", e.kind()))?;
    if checksum(&payload) != header[20..24] {
        bail!("{cmd} bad checksum");
    }
    Ok((cmd, payload))
}

fn checksum(payload: &[u8]) -> [u8; 4] {
    let h = Sha256::digest(Sha256::digest(payload));
    [h[0], h[1], h[2], h[3]]
}

pub fn encode(magic: [u8; 4], cmd: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(24 + payload.len());
    out.extend_from_slice(&magic);
    let mut name = [0u8; 12];
    name[..cmd.len()].copy_from_slice(cmd.as_bytes());
    out.extend_from_slice(&name);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&checksum(payload));
    out.extend_from_slice(payload);
    out
}

fn version_payload(addr_you: SocketAddr, start_height: i32) -> Vec<u8> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    let mut p = Vec::with_capacity(128);
    p.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    p.extend_from_slice(&0u64.to_le_bytes()); // services: NODE_NONE
    p.extend_from_slice(&now.to_le_bytes());
    write_net_addr(&mut p, Some(addr_you));
    write_net_addr(&mut p, None);
    p.extend_from_slice(&rand::random::<u64>().to_le_bytes());
    let ua = user_agent();
    write_compact_size(&mut p, ua.len() as u64);
    p.extend_from_slice(ua.as_bytes());
    p.extend_from_slice(&start_height.to_le_bytes());
    p.push(0); // fRelay = false: we don't want transaction announcements.
    // mnauth challenge and fMasternode are optional trailing fields; leaving them out is the
    // same as "not a masternode".
    p
}

/// Pre-31402 CAddress serialization (no time): services, 16-byte IPv6 (v4-mapped), port big-endian.
fn write_net_addr(p: &mut Vec<u8>, addr: Option<SocketAddr>) {
    p.extend_from_slice(&0u64.to_le_bytes());
    match addr {
        Some(a) => {
            let v6 = match a.ip() {
                IpAddr::V4(v4) => v4.to_ipv6_mapped(),
                IpAddr::V6(v6) => v6,
            };
            p.extend_from_slice(&v6.octets());
            p.extend_from_slice(&a.port().to_be_bytes());
        }
        None => p.extend_from_slice(&[0u8; 18]),
    }
}

fn write_compact_size(p: &mut Vec<u8>, n: u64) {
    match n {
        0..=0xfc => p.push(n as u8),
        0xfd..=0xffff => {
            p.push(0xfd);
            p.extend_from_slice(&(n as u16).to_le_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            p.push(0xfe);
            p.extend_from_slice(&(n as u32).to_le_bytes());
        }
        _ => {
            p.push(0xff);
            p.extend_from_slice(&n.to_le_bytes());
        }
    }
}

struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.buf.len() < n {
            bail!("truncated");
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn compact_size(&mut self) -> Result<u64> {
        Ok(match self.u8()? {
            0xfd => u16::from_le_bytes(self.take(2)?.try_into().unwrap()) as u64,
            0xfe => self.u32()? as u64,
            0xff => self.u64()?,
            n => n as u64,
        })
    }
    fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

pub fn parse_version(payload: &[u8]) -> Result<Version> {
    let mut r = Reader { buf: payload };
    let protocol = r.i32()?;
    let services = r.u64()?;
    r.take(8)?; // time
    r.take(26)?; // addr_recv
    let mut user_agent = String::new();
    let mut start_height = -1;
    if !r.is_empty() {
        r.take(26)?; // addr_from
        r.take(8)?; // nonce
    }
    if !r.is_empty() {
        let len = r.compact_size()?;
        if len as usize > MAX_SUBVERSION_LENGTH {
            bail!("user agent too long ({len})");
        }
        user_agent = sanitize(r.take(len as usize)?);
    }
    if !r.is_empty() {
        start_height = r.i32()?;
    }
    Ok(Version { protocol, services, user_agent, start_height, latency_ms: 0 })
}

/// Same idea as Core's SanitizeString: keep printable ASCII only.
fn sanitize(bytes: &[u8]) -> String {
    bytes.iter().filter(|b| (0x20..0x7f).contains(*b)).map(|&b| b as char).collect()
}

pub fn parse_addr(payload: &[u8]) -> Result<Vec<SocketAddr>> {
    let mut r = Reader { buf: payload };
    let n = r.compact_size()?;
    if n > MAX_ADDRS {
        bail!("addr with {n} entries");
    }
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        r.take(4)?; // time
        r.take(8)?; // services
        let ip = Ipv6Addr::from(<[u8; 16]>::try_from(r.take(16)?).unwrap());
        let port = u16::from_be_bytes(r.take(2)?.try_into().unwrap());
        let ip = ip.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(ip));
        out.push(SocketAddr::new(ip, port));
    }
    Ok(out)
}

/// Publicly routable unicast address we could plausibly reach.
pub fn is_routable(addr: &SocketAddr) -> bool {
    if addr.port() == 0 {
        return false;
    }
    match addr.ip() {
        IpAddr::V4(ip) => {
            let o = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || ip.is_multicast()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xc0) == 64) // CGNAT 100.64/10
                || (o[0] == 198 && (o[1] & 0xfe) == 18) // benchmarking 198.18/15
                || o[0] >= 240)
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            (s[0] & 0xe000) == 0x2000 // global unicast 2000::/3
                && !(s[0] == 0x2001 && s[1] == 0x0db8) // documentation
                && !(s[0] == 0x2002) // 6to4
                && !(s[0] == 0x2001 && s[1] == 0) // Teredo
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_roundtrip() {
        let addr: SocketAddr = "1.2.3.4:9999".parse().unwrap();
        let payload = version_payload(addr, 123);
        let v = parse_version(&payload).unwrap();
        assert_eq!(v.protocol, PROTOCOL_VERSION);
        assert_eq!(v.user_agent, user_agent());
        assert_eq!(v.start_height, 123);
    }

    #[test]
    fn checksum_of_empty_payload() {
        // sha256d("") starts with 5df6e0e2, the well-known verack checksum.
        assert_eq!(checksum(&[]), [0x5d, 0xf6, 0xe0, 0xe2]);
        let msg = encode(Network::Mainnet.magic(), "verack", &[]);
        assert_eq!(msg.len(), 24);
        assert_eq!(&msg[4..10], b"verack");
    }

    #[test]
    fn addr_parsing() {
        let mut p = vec![2u8];
        for (ip, port) in [([1u8, 2, 3, 4], 9999u16), ([5, 6, 7, 8], 19999)] {
            p.extend_from_slice(&0u32.to_le_bytes());
            p.extend_from_slice(&1u64.to_le_bytes());
            p.extend_from_slice(&std::net::Ipv4Addr::from(ip).to_ipv6_mapped().octets());
            p.extend_from_slice(&port.to_be_bytes());
        }
        let addrs = parse_addr(&p).unwrap();
        assert_eq!(addrs, vec!["1.2.3.4:9999".parse().unwrap(), "5.6.7.8:19999".parse().unwrap()]);
    }

    #[test]
    fn routability() {
        for (a, ok) in [
            ("8.8.8.8:9999", true),
            ("10.0.0.1:9999", false),
            ("192.168.1.1:9999", false),
            ("100.64.1.1:9999", false),
            ("127.0.0.1:9999", false),
            ("[2a01:4f8::1]:9999", true),
            ("[fd87:d87e:eb43::1]:9999", false),
            ("8.8.8.8:0", false),
        ] {
            assert_eq!(is_routable(&a.parse().unwrap()), ok, "{a}");
        }
    }
}

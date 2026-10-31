//! Just enough of the Dash P2P protocol (v1 transport) to handshake with a node, read its
//! `version` message and optionally ask it for addresses.
//!
//! We connect as an ordinary, non-masternode, non-relaying peer, so the remote treats us like any
//! inbound light client: it answers our `version`, and answers one `getaddr` per connection. We
//! announce `sendaddrv2` (BIP155), so the reply can include onion, I2P and CJDNS addresses.

use std::net::{IpAddr, Ipv6Addr};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::addr::{self, Host, PeerAddr};
use crate::network::Network;
use crate::transport::{self, Route};

/// Protocol version we announce. Matches Dash Core v24; peers settle on min(ours, theirs).
pub const PROTOCOL_VERSION: i32 = 70242;
/// Largest message we accept. A full 1000-entry `addr` is ~30 KB and a full `addrv2` of 32-byte
/// (onion, I2P) addresses ~49 KB; anything bigger isn't for us.
const MAX_PAYLOAD: usize = 64 * 1024;
/// First protocol version that knows `sendaddrv2`/`addrv2` (Dash Core's ADDRV2_PROTO_VERSION).
const ADDRV2_PROTO_VERSION: i32 = 70223;
/// Dash Core's CNetAddr::MAX_ADDRV2_SIZE: longer addresses make the whole message invalid.
const MAX_ADDRV2_SIZE: u64 = 512;
/// Messages read while waiting for the getaddr reply before giving up on it.
const MAX_GETADDR_MESSAGES: usize = 64;
const MAX_SUBVERSION_LENGTH: usize = 256;
const MAX_ADDRS: u64 = 1000;

pub fn user_agent() -> String {
    format!("/dash-nodes:{}(nodes.dashhq.org)/", env!("CARGO_PKG_VERSION"))
}

#[derive(Clone, Debug)]
pub struct Timeouts {
    /// TCP connect, or for a proxy the whole SOCKS exchange (Tor circuit, I2P tunnel).
    pub connect: Duration,
    /// From connected until we have the peer's `version`.
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
    /// The version message's relay flag: whether the node wants transactions announced to it. Off
    /// with `blocksonly=1`. Absent (very old peers) counts as on, as in Dash Core.
    pub relay: bool,
    pub latency_ms: u32,
}

#[derive(Debug)]
pub struct Probe {
    pub version: Version,
    pub addrs: Vec<PeerAddr>,
}

/// Connects over `route`, exchanges versions and, if `getaddr` is set, collects the peer's address
/// sample (at most 1000 addresses). The whole exchange, writes included, is bounded by the
/// timeouts, which are per network (see transport::default_limits).
pub async fn probe(
    net: Network,
    addr: &PeerAddr,
    route: &Route,
    start_height: i32,
    getaddr: bool,
    t: &Timeouts,
) -> Result<Probe> {
    let budget = t.connect + t.handshake + if getaddr { t.getaddr } else { Duration::ZERO } + Duration::from_secs(2);
    timeout(budget, probe_inner(net, addr, route, start_height, getaddr, t)).await.map_err(|_| anyhow!("timeout"))?
}

async fn probe_inner(
    net: Network,
    addr: &PeerAddr,
    route: &Route,
    start_height: i32,
    getaddr: bool,
    t: &Timeouts,
) -> Result<Probe> {
    let started = Instant::now();
    let mut stream = transport::connect(route, addr, t.connect).await?;

    let magic = net.magic();
    stream.write_all(&encode(magic, "version", &version_payload(addr, start_height))).await?;

    let deadline = Instant::now() + t.handshake;
    let version = loop {
        let (cmd, payload) = read_before(&mut stream, magic, deadline).await?;
        if cmd == "version" {
            let mut v = parse_version(&payload)?;
            v.latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
            break v;
        }
    };
    // BIP155: sendaddrv2 must arrive between version and verack. Like Dash Core, only send it to
    // peers new enough to know it.
    if version.protocol >= ADDRV2_PROTO_VERSION {
        stream.write_all(&encode(magic, "sendaddrv2", &[])).await?;
    }
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
                let batch = match cmd.as_str() {
                    "addr" => parse_addr(&payload)?,
                    "addrv2" => parse_addrv2(&payload)?,
                    _ => continue,
                };
                // A lone address is ordinary relay; the getaddr reply is the big batch.
                let done = batch.len() > 1;
                let room = MAX_ADDRS as usize - addrs.len();
                addrs.extend(batch.into_iter().take(room));
                if done || addrs.len() >= MAX_ADDRS as usize {
                    return Ok(());
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
    timeout(left, read_message(stream, magic, MAX_PAYLOAD)).await.map_err(|_| anyhow!("timeout"))?
}

/// Reads one message, refusing payloads over `max_payload` bytes before reading them. Not
/// cancel-safe: a timeout mid-message leaves the stream unusable.
pub async fn read_message(stream: &mut TcpStream, magic: [u8; 4], max_payload: usize) -> Result<(String, Vec<u8>)> {
    let mut header = [0u8; 24];
    stream.read_exact(&mut header).await.map_err(|e| anyhow!("read: {}", e.kind()))?;
    let (cmd, len) = decode_header(&header, magic, max_payload)?;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await.map_err(|e| anyhow!("read: {}", e.kind()))?;
    verify_checksum(&cmd, &header, &payload)?;
    Ok((cmd, payload))
}

/// Checks a 24-byte message header (magic, ASCII command, payload size) and returns the command
/// and payload length.
pub fn decode_header(header: &[u8], magic: [u8; 4], max_payload: usize) -> Result<(String, usize)> {
    if header[..4] != magic {
        bail!("bad magic {:02x?}", &header[..4]);
    }
    let cmd_bytes = &header[4..16];
    let end = cmd_bytes.iter().position(|&b| b == 0).unwrap_or(12);
    let cmd = std::str::from_utf8(&cmd_bytes[..end]).context("command not ascii")?.to_string();
    let len = u32::from_le_bytes(header[16..20].try_into().unwrap()) as usize;
    if len > max_payload {
        bail!("{cmd} payload too large ({len})");
    }
    Ok((cmd, len))
}

pub fn verify_checksum(cmd: &str, header: &[u8], payload: &[u8]) -> Result<()> {
    if checksum(payload) != header[20..24] {
        bail!("{cmd} bad checksum");
    }
    Ok(())
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

pub fn version_payload(addr_you: &PeerAddr, start_height: i32) -> Vec<u8> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    let mut p = Vec::with_capacity(128);
    p.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    p.extend_from_slice(&0u64.to_le_bytes()); // services: NODE_NONE
    p.extend_from_slice(&now.to_le_bytes());
    write_net_addr(&mut p, addr_you.public_ip().map(|ip| (ip, addr_you.port)));
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
/// Addresses v1 can't express (onion, I2P, CJDNS) go out as all zeros, as in Dash Core.
fn write_net_addr(p: &mut Vec<u8>, addr: Option<(IpAddr, u16)>) {
    p.extend_from_slice(&0u64.to_le_bytes());
    match addr {
        Some((ip, port)) => {
            let v6 = match ip {
                IpAddr::V4(v4) => v4.to_ipv6_mapped(),
                IpAddr::V6(v6) => v6,
            };
            p.extend_from_slice(&v6.octets());
            p.extend_from_slice(&port.to_be_bytes());
        }
        None => p.extend_from_slice(&[0u8; 18]),
    }
}

pub fn write_compact_size(p: &mut Vec<u8>, n: u64) {
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

/// Bounds-checked cursor over an untrusted payload.
pub struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf }
    }
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.buf.len() < n {
            bail!("truncated");
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    /// Bitcoin CompactSize; like Dash Core, non-canonical (over-long) encodings are rejected.
    pub fn compact_size(&mut self) -> Result<u64> {
        let (n, min) = match self.u8()? {
            0xfd => (u16::from_le_bytes(self.take(2)?.try_into().unwrap()) as u64, 0xfd),
            0xfe => (self.u32()? as u64, 0x1_0000),
            0xff => (self.u64()?, 0x1_0000_0000),
            n => (n as u64, 0),
        };
        if n < min {
            bail!("non-canonical compact size");
        }
        Ok(n)
    }
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub fn hash(&mut self) -> Result<[u8; 32]> {
        Ok(self.take(32)?.try_into().unwrap())
    }
    /// A CompactSize count of items at least `min_item` bytes each, checked against what's left so a
    /// hostile count can't make us allocate or loop far beyond the payload.
    pub fn count(&mut self, min_item: usize) -> Result<usize> {
        let n = self.compact_size()?;
        if n.saturating_mul(min_item.max(1) as u64) > self.buf.len() as u64 {
            bail!("count {n} exceeds payload");
        }
        Ok(n as usize)
    }
    /// A CompactSize-prefixed byte string.
    pub fn var_bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.count(1)?;
        self.take(n)
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
    let mut relay = true;
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
    if !r.is_empty() {
        relay = r.u8()? != 0;
    }
    // Dash Core follows with an mnauth challenge and fMasternode, which we don't need.
    Ok(Version { protocol, services, user_agent, start_height, relay, latency_ms: 0 })
}

/// Same idea as Core's SanitizeString: keep printable ASCII only.
fn sanitize(bytes: &[u8]) -> String {
    bytes.iter().filter(|b| (0x20..0x7f).contains(*b)).map(|&b| b as char).collect()
}

/// `addr` (v1): IPv4 and IPv6 only. An fc00::/8 address here is plain (unroutable) IPv6: CJDNS
/// addresses can't be sent in v1 and only appear in `addrv2`.
pub fn parse_addr(payload: &[u8]) -> Result<Vec<PeerAddr>> {
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
        let host = ip.to_ipv4_mapped().map_or(Host::V6(ip), Host::V4);
        out.push(PeerAddr { host, port });
    }
    Ok(out)
}

/// `addrv2` (BIP155): time, services as CompactSize, network ID, length-prefixed address, port.
/// Entries on unknown or retired networks are skipped; a size that doesn't fit its network, or
/// any address longer than 512 bytes, invalidates the message, as in Dash Core.
pub fn parse_addrv2(payload: &[u8]) -> Result<Vec<PeerAddr>> {
    let mut r = Reader { buf: payload };
    let n = r.compact_size()?;
    if n > MAX_ADDRS {
        bail!("addrv2 with {n} entries");
    }
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        r.take(4)?; // time
        r.compact_size()?; // services
        let net_id = r.u8()?;
        let len = r.compact_size()?;
        if len > MAX_ADDRV2_SIZE {
            bail!("addrv2 address too long ({len})");
        }
        let bytes = r.take(len as usize)?;
        let port = u16::from_be_bytes(r.take(2)?.try_into().unwrap());
        out.extend(addr::from_bip155(net_id, bytes, port)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::AddrNet;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn version_roundtrip() {
        let addr: PeerAddr = "1.2.3.4:9999".parse().unwrap();
        let payload = version_payload(&addr, 123);
        let v = parse_version(&payload).unwrap();
        assert_eq!(v.protocol, PROTOCOL_VERSION);
        assert_eq!(v.user_agent, user_agent());
        assert_eq!(v.start_height, 123);
        // We announce fRelay = false.
        assert!(!v.relay);
        // addr_recv: services, then the v4-mapped address and port.
        assert_eq!(payload[20 + 8..20 + 26], [&[0u8; 10][..], &[0xff, 0xff, 1, 2, 3, 4], &[0x27, 0x0f]].concat()[..]);
        // An onion peer's address can't be expressed in v1: zeros.
        let onion: PeerAddr = "pg6mmjiyjmcrsslvykfwnntlaru7p5svn6y2ymmju6nubxndf4pscryd.onion:9999".parse().unwrap();
        assert_eq!(version_payload(&onion, 1)[20 + 8..20 + 26], [0u8; 18]);
    }

    #[test]
    fn version_relay_flag() {
        let addr: PeerAddr = "1.2.3.4:9999".parse().unwrap();
        let mut payload = version_payload(&addr, 1);
        // relay = 1, then the trailing mnauth challenge and fMasternode a Dash Core node sends.
        *payload.last_mut().unwrap() = 1;
        payload.extend_from_slice(&[7u8; 32]);
        payload.push(0);
        assert!(parse_version(&payload).unwrap().relay);
        // A message that ends after the start height relays, as Dash Core assumes.
        payload.truncate(payload.len() - 34);
        assert!(parse_version(&payload).unwrap().relay);
    }

    #[test]
    fn checksum_of_empty_payload() {
        // sha256d("") starts with 5df6e0e2, the well-known verack checksum.
        assert_eq!(checksum(&[]), [0x5d, 0xf6, 0xe0, 0xe2]);
        let msg = encode(Network::Mainnet.magic(), "verack", &[]);
        assert_eq!(msg.len(), 24);
        assert_eq!(&msg[4..10], b"verack");
        assert_eq!(&encode(Network::Mainnet.magic(), "sendaddrv2", &[])[4..16], b"sendaddrv2\0\0");
    }

    #[test]
    fn addr_parsing() {
        let mut p = vec![3u8];
        for (ip, port) in [
            (std::net::Ipv4Addr::from([1u8, 2, 3, 4]).to_ipv6_mapped(), 9999u16),
            (std::net::Ipv4Addr::from([5, 6, 7, 8]).to_ipv6_mapped(), 19999),
            ("fc00::1".parse().unwrap(), 9999),
        ] {
            p.extend_from_slice(&0u32.to_le_bytes());
            p.extend_from_slice(&1u64.to_le_bytes());
            p.extend_from_slice(&ip.octets());
            p.extend_from_slice(&port.to_be_bytes());
        }
        let addrs = parse_addr(&p).unwrap();
        assert_eq!(addrs[..2], ["1.2.3.4:9999".parse().unwrap(), "5.6.7.8:19999".parse().unwrap()]);
        // fc00::/8 in v1 is ULA IPv6, never CJDNS, and not routable.
        assert_eq!(addrs[2].net(), AddrNet::Ipv6);
        assert!(!addrs[2].is_routable());
    }

    fn addrv2_entry(net_id: u8, addr: &[u8], port: u16) -> Vec<u8> {
        let mut e = 0x6560_0000u32.to_le_bytes().to_vec(); // time
        e.extend_from_slice(&[0xfd, 0x05, 0x04]); // services 0x0405 as CompactSize
        e.push(net_id);
        e.push(addr.len() as u8);
        e.extend_from_slice(addr);
        e.extend_from_slice(&port.to_be_bytes());
        e
    }

    #[test]
    fn addrv2_parsing() {
        // Dash Core's net_tests vectors, plus TORv2 and an unknown network, which are skipped.
        let entries = [
            addrv2_entry(1, &hex("01020304"), 9999),
            addrv2_entry(2, &hex("2a0102030405060708090a0b0c0d0e0f"), 9999),
            addrv2_entry(3, &hex("f1f2f3f4f5f6f7f8f9fa"), 9999),
            addrv2_entry(4, &hex("79bcc625184b05194975c28b66b66b0469f7f6556fb1ac3189a79b40dda32f1f"), 9999),
            addrv2_entry(5, &hex("a2894dabaec08c0051a481a6dac88b64f98232ae42d4b6fd2fa81952dfe36a87"), 0),
            addrv2_entry(6, &hex("fc000001000200030004000500060007"), 9999),
            addrv2_entry(0xaa, &[7; 40], 1),
        ];
        let mut p = vec![entries.len() as u8];
        entries.iter().for_each(|e| p.extend_from_slice(e));
        let got: Vec<String> = parse_addrv2(&p).unwrap().iter().map(|a| a.to_string()).collect();
        assert_eq!(
            got,
            [
                "1.2.3.4:9999",
                "[2a01:203:405:607:809:a0b:c0d:e0f]:9999",
                "pg6mmjiyjmcrsslvykfwnntlaru7p5svn6y2ymmju6nubxndf4pscryd.onion:9999",
                "ukeu3k5oycgaauneqgtnvselmt4yemvoilkln7jpvamvfx7dnkdq.b32.i2p:0",
                "[fc00:1:2:3:4:5:6:7]:9999",
            ]
        );
    }

    #[test]
    fn addrv2_rejects_malformed() {
        let one = |e: Vec<u8>| [vec![1u8], e].concat();
        // Size doesn't match the network.
        assert!(parse_addrv2(&one(addrv2_entry(1, &[1, 2, 3, 4, 5], 1))).is_err());
        assert!(parse_addrv2(&one(addrv2_entry(4, &[0; 31], 1))).is_err());
        // Address longer than 512 bytes, even on an unknown network.
        let mut long = 0u32.to_le_bytes().to_vec();
        long.extend_from_slice(&[0, 0xaa, 0xfd, 0x01, 0x02]); // services 0, net 0xaa, len 513
        long.extend_from_slice(&[0; 515]);
        assert!(parse_addrv2(&one(long)).unwrap_err().to_string().contains("too long"));
        // Truncated entry, too many entries, non-canonical CompactSize.
        let e = addrv2_entry(1, &[1, 2, 3, 4], 1);
        assert!(parse_addrv2(&one(e[..e.len() - 1].to_vec())).is_err());
        assert!(parse_addrv2(&[0xfd, 0xe9, 0x03]).unwrap_err().to_string().contains("1001"));
        assert!(parse_addrv2(&[0xfd, 0x01, 0x00]).unwrap_err().to_string().contains("non-canonical"));
    }
}

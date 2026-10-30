//! Peer addresses on every network Dash nodes use (BIP155): IPv4, IPv6, Tor v3 onion services, I2P
//! and CJDNS. Text forms match what Dash Core prints (`getpeerinfo`, `getnodeaddresses`, ProTx
//! `addresses`), so they can be used as keys and compared with dashd output:
//!
//! - `1.2.3.4:9999`, `[2a01:4f8::1]:9999`
//! - `<56 base32>.onion:9999` (pubkey, checksum, version 3)
//! - `<52 base32>.b32.i2p:0` (SAM 3.1 has no ports, so I2P addresses always use port 0)
//! - `[fc00:1:2:3:4:5:6:7]:9999` (CJDNS is IPv6 inside fc00::/8)

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::str::FromStr;

use anyhow::{Result, bail};
use serde::Serialize;
use sha3::{Digest, Sha3_256};

/// The address network of a peer (not to be confused with mainnet/testnet, see `Network`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AddrNet {
    Ipv4,
    Ipv6,
    Onion,
    I2p,
    Cjdns,
}

impl AddrNet {
    /// Every network, in the order the API lists them.
    pub const ALL: [AddrNet; 5] = [AddrNet::Ipv4, AddrNet::Ipv6, AddrNet::Onion, AddrNet::I2p, AddrNet::Cjdns];

    pub fn name(self) -> &'static str {
        match self {
            AddrNet::Ipv4 => "ipv4",
            AddrNet::Ipv6 => "ipv6",
            AddrNet::Onion => "onion",
            AddrNet::I2p => "i2p",
            AddrNet::Cjdns => "cjdns",
        }
    }

    /// Position in `ALL`, for per-network arrays.
    pub fn index(self) -> usize {
        self as usize
    }
}

impl fmt::Display for AddrNet {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// BIP155 network IDs. 3 (TORv2) is retired; Dash Core skips it like any unknown ID.
pub mod bip155 {
    pub const IPV4: u8 = 1;
    pub const IPV6: u8 = 2;
    pub const TORV3: u8 = 4;
    pub const I2P: u8 = 5;
    pub const CJDNS: u8 = 6;
}

/// Tor v3 public key, I2P destination hash: both 32 bytes.
pub const ADDR_TORV3_SIZE: usize = 32;
pub const ADDR_I2P_SIZE: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Host {
    V4(Ipv4Addr),
    V6(Ipv6Addr),
    /// Tor v3 ed25519 public key.
    Onion([u8; ADDR_TORV3_SIZE]),
    /// SHA-256 of the I2P destination.
    I2p([u8; ADDR_I2P_SIZE]),
    /// Always inside fc00::/8.
    Cjdns(Ipv6Addr),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PeerAddr {
    pub host: Host,
    pub port: u16,
}

/// What to ask a SOCKS5 proxy to connect to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SocksTarget {
    Ip(SocketAddr),
    Domain(String, u16),
}

pub fn is_cjdns_ip(ip: &Ipv6Addr) -> bool {
    ip.octets()[0] == 0xfc
}

impl PeerAddr {
    /// Classifies an IP socket address. IPv4-mapped IPv6 becomes IPv4; fc00::/8 is CJDNS, as it is
    /// in Dash Core with -cjdnsreachable (as plain IPv6 it would be unroutable ULA space anyway).
    pub fn from_socket(sa: SocketAddr) -> PeerAddr {
        let host = match sa.ip() {
            IpAddr::V4(ip) => Host::V4(ip),
            IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
                Some(v4) => Host::V4(v4),
                None if is_cjdns_ip(&ip) => Host::Cjdns(ip),
                None => Host::V6(ip),
            },
        };
        PeerAddr { host, port: sa.port() }
    }

    pub fn net(&self) -> AddrNet {
        match self.host {
            Host::V4(_) => AddrNet::Ipv4,
            Host::V6(_) => AddrNet::Ipv6,
            Host::Onion(_) => AddrNet::Onion,
            Host::I2p(_) => AddrNet::I2p,
            Host::Cjdns(_) => AddrNet::Cjdns,
        }
    }

    /// Public internet IP, for geolocation. CJDNS addresses say nothing about location.
    pub fn public_ip(&self) -> Option<IpAddr> {
        match self.host {
            Host::V4(ip) => Some(IpAddr::V4(ip)),
            Host::V6(ip) => Some(IpAddr::V6(ip)),
            _ => None,
        }
    }

    /// The address to connect a socket to, for networks reached over IP (CJDNS included).
    pub fn socket_addr(&self) -> Option<SocketAddr> {
        match self.host {
            Host::V4(ip) => Some(SocketAddr::new(IpAddr::V4(ip), self.port)),
            Host::V6(ip) | Host::Cjdns(ip) => Some(SocketAddr::new(IpAddr::V6(ip), self.port)),
            Host::Onion(_) | Host::I2p(_) => None,
        }
    }

    pub fn socks_target(&self) -> SocksTarget {
        match self.socket_addr() {
            Some(sa) => SocksTarget::Ip(sa),
            None => SocksTarget::Domain(self.host_string(), self.port),
        }
    }

    /// The host part as Dash Core prints it (no brackets, no port).
    pub fn host_string(&self) -> String {
        match &self.host {
            Host::V4(ip) => ip.to_string(),
            Host::V6(ip) | Host::Cjdns(ip) => ip.to_string(),
            Host::Onion(pk) => onion_host(pk),
            Host::I2p(h) => format!("{}.b32.i2p", base32_encode(h)),
        }
    }

    /// An address we could plausibly reach on its network: publicly routable IP space, a valid
    /// CJDNS address, any onion service, an I2P destination on the only port SAM 3.1 supports.
    pub fn is_routable(&self) -> bool {
        match self.host {
            Host::V4(ip) => self.port != 0 && is_routable_v4(&ip),
            Host::V6(ip) => self.port != 0 && is_routable_v6(&ip),
            Host::Cjdns(ip) => self.port != 0 && is_cjdns_ip(&ip),
            Host::Onion(_) => self.port != 0,
            Host::I2p(_) => self.port == 0,
        }
    }

    /// Parses Dash Core's `getnodeaddresses` entry form: a bare host, IPv6 and CJDNS without
    /// brackets. dashd only keeps fc00::/8 addresses at all when they're CJDNS (-cjdnsreachable);
    /// as IPv6 they'd be unroutable ULA space.
    pub fn from_parts(host: &str, port: u16) -> Option<PeerAddr> {
        let host = host.trim_matches(|c| c == '[' || c == ']');
        match host.parse::<IpAddr>() {
            Ok(ip) => Some(PeerAddr::from_socket(SocketAddr::new(ip, port))),
            Err(_) => Some(PeerAddr { host: parse_special_host(host)?, port }),
        }
    }
}

fn is_routable_v4(ip: &Ipv4Addr) -> bool {
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

fn is_routable_v6(ip: &Ipv6Addr) -> bool {
    let s = ip.segments();
    (s[0] & 0xe000) == 0x2000 // global unicast 2000::/3
        && !(s[0] == 0x2001 && s[1] == 0x0db8) // documentation
        && s[0] != 0x2002 // 6to4
        && !(s[0] == 0x2001 && s[1] == 0) // Teredo
}

impl fmt::Display for PeerAddr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.host {
            Host::V6(_) | Host::Cjdns(_) => write!(f, "[{}]:{}", self.host_string(), self.port),
            _ => write!(f, "{}:{}", self.host_string(), self.port),
        }
    }
}

impl FromStr for PeerAddr {
    type Err = anyhow::Error;

    /// Accepts the `Display` forms; onion and I2P names case-insensitively.
    fn from_str(s: &str) -> Result<PeerAddr> {
        if let Ok(sa) = s.parse::<SocketAddr>() {
            return Ok(PeerAddr::from_socket(sa));
        }
        let Some((host, port)) = s.rsplit_once(':') else { bail!("{s}: missing port") };
        let Ok(port) = port.parse::<u16>() else { bail!("{s}: bad port") };
        match parse_special_host(host) {
            Some(host) => Ok(PeerAddr { host, port }),
            None => bail!("{s}: not an IP, v3 .onion or .b32.i2p address"),
        }
    }
}

fn parse_special_host(host: &str) -> Option<Host> {
    let lower = host.to_ascii_lowercase();
    if let Some(b32) = lower.strip_suffix(".onion") {
        // pubkey (32) | checksum (2) | version (1) = 35 bytes = 56 base32 characters.
        if b32.len() != 56 {
            return None; // includes retired 16-character v2 names
        }
        let raw = base32_decode(b32)?;
        let pk: [u8; 32] = raw[..32].try_into().ok()?;
        if raw[34] != 3 || raw[32..34] != onion_checksum(&pk) {
            return None;
        }
        return Some(Host::Onion(pk));
    }
    if let Some(b32) = lower.strip_suffix(".b32.i2p") {
        if b32.len() != 52 {
            return None;
        }
        return Some(Host::I2p(base32_decode(b32)?.try_into().ok()?));
    }
    None
}

/// Tor v3: CHECKSUM = SHA3-256(".onion checksum" | PUBKEY | VERSION)[:2] (rend-spec-v3).
fn onion_checksum(pubkey: &[u8; 32]) -> [u8; 2] {
    let mut h = Sha3_256::new();
    h.update(b".onion checksum");
    h.update(pubkey);
    h.update([3u8]);
    let d = h.finalize();
    [d[0], d[1]]
}

fn onion_host(pubkey: &[u8; 32]) -> String {
    let mut raw = Vec::with_capacity(35);
    raw.extend_from_slice(pubkey);
    raw.extend_from_slice(&onion_checksum(pubkey));
    raw.push(3);
    format!("{}.onion", base32_encode(&raw))
}

const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// RFC 4648 base32, lowercase, unpadded (what Dash Core prints for onion and I2P names).
pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in data {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32[((acc >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(BASE32[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// Inverse of `base32_encode`. Like Dash Core's DecodeBase32, leftover bits must be zero, so every
/// address has exactly one text form.
pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = BASE32.iter().position(|&x| x == c.to_ascii_lowercase())? as u32;
        acc = ((acc << 5) | v) & 0xfff;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    (bits < 5 && acc & ((1 << bits) - 1) == 0).then_some(out)
}

/// Builds an address from a BIP155 network ID and its raw bytes. `Ok(None)` is an address Dash
/// Core also ignores (unknown or retired network, IPv4 or TORv2 embedded in IPv6, CJDNS outside
/// fc00::/8); `Err` is a size that doesn't match the network, which Dash Core treats as a
/// malformed message.
pub fn from_bip155(net_id: u8, bytes: &[u8], port: u16) -> Result<Option<PeerAddr>> {
    let want = match net_id {
        bip155::IPV4 => 4,
        bip155::IPV6 | bip155::CJDNS => 16,
        bip155::TORV3 => ADDR_TORV3_SIZE,
        bip155::I2P => ADDR_I2P_SIZE,
        _ => return Ok(None),
    };
    if bytes.len() != want {
        bail!("BIP155 network {net_id} address with length {} (should be {want})", bytes.len());
    }
    let host = match net_id {
        bip155::IPV4 => Host::V4(Ipv4Addr::from(<[u8; 4]>::try_from(bytes).unwrap())),
        bip155::IPV6 => {
            let ip = Ipv6Addr::from(<[u8; 16]>::try_from(bytes).unwrap());
            // ::ffff:0:0/96 (IPv4) and fd87:d87e:eb43::/48 (TORv2) must not be embedded in addrv2.
            if ip.to_ipv4_mapped().is_some() || bytes.starts_with(&[0xfd, 0x87, 0xd8, 0x7e, 0xeb, 0x43]) {
                return Ok(None);
            }
            Host::V6(ip)
        }
        bip155::TORV3 => Host::Onion(bytes.try_into().unwrap()),
        bip155::I2P => Host::I2p(bytes.try_into().unwrap()),
        _ => {
            let ip = Ipv6Addr::from(<[u8; 16]>::try_from(bytes).unwrap());
            if !is_cjdns_ip(&ip) {
                return Ok(None);
            }
            Host::Cjdns(ip)
        }
    };
    Ok(Some(PeerAddr { host, port }))
}

/// Classifies a stored `nodes.addr` (or ProTx service) string; None if it doesn't parse.
pub fn net_of(addr: &str) -> Option<AddrNet> {
    addr.parse::<PeerAddr>().ok().map(|a| a.net())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    // Vectors from Dash Core's src/test/net_tests.cpp (cnetaddr_unserialize_v2, cnetaddr_basic).
    const TORV3_HEX: &str = "79bcc625184b05194975c28b66b66b0469f7f6556fb1ac3189a79b40dda32f1f";
    const TORV3_NAME: &str = "pg6mmjiyjmcrsslvykfwnntlaru7p5svn6y2ymmju6nubxndf4pscryd.onion";
    const I2P_HEX: &str = "a2894dabaec08c0051a481a6dac88b64f98232ae42d4b6fd2fa81952dfe36a87";
    const I2P_NAME: &str = "ukeu3k5oycgaauneqgtnvselmt4yemvoilkln7jpvamvfx7dnkdq.b32.i2p";

    #[test]
    fn bip155_vectors_print_like_dash_core() {
        let onion = from_bip155(bip155::TORV3, &hex(TORV3_HEX), 9999).unwrap().unwrap();
        assert_eq!(onion.to_string(), format!("{TORV3_NAME}:9999"));
        assert_eq!(onion.net(), AddrNet::Onion);
        let i2p = from_bip155(bip155::I2P, &hex(I2P_HEX), 0).unwrap().unwrap();
        assert_eq!(i2p.to_string(), format!("{I2P_NAME}:0"));
        let cjdns = from_bip155(bip155::CJDNS, &hex("fc000001000200030004000500060007"), 9999).unwrap().unwrap();
        assert_eq!(cjdns.to_string(), "[fc00:1:2:3:4:5:6:7]:9999");
        assert_eq!(cjdns.net(), AddrNet::Cjdns);
        let v4 = from_bip155(bip155::IPV4, &hex("01020304"), 9999).unwrap().unwrap();
        assert_eq!(v4.to_string(), "1.2.3.4:9999");
        let v6 = from_bip155(bip155::IPV6, &hex("2a0102030405060708090a0b0c0d0e0f"), 9999).unwrap().unwrap();
        assert_eq!(v6.to_string(), "[2a01:203:405:607:809:a0b:c0d:e0f]:9999");
    }

    #[test]
    fn onion_name_from_serialized_pubkey() {
        // cnetaddr_serialize_v2: this name serializes as 04 20 53cd5648...
        let a: PeerAddr = "kpgvmscirrdqpekbqjsvw5teanhatztpp2gl6eee4zkowvwfxwenqaid.onion:9999".parse().unwrap();
        assert_eq!(
            a.host,
            Host::Onion(hex("53cd5648488c4707914182655b7664034e09e66f7e8cbf1084e654eb56c5bd88").try_into().unwrap())
        );
    }

    #[test]
    fn bip155_rejects_and_skips() {
        // Wrong size for a known network: malformed.
        assert!(from_bip155(bip155::IPV4, &hex("0102030405"), 1).is_err());
        assert!(from_bip155(bip155::IPV6, &hex("00000000"), 1).is_err());
        assert!(from_bip155(bip155::TORV3, &[], 1).is_err());
        assert!(from_bip155(bip155::I2P, &hex("000000"), 1).is_err());
        assert!(from_bip155(bip155::CJDNS, &hex("00"), 1).is_err());
        // Ignored: TORv2, unknown IDs, embedded IPv4/TORv2, CJDNS outside fc00::/8.
        assert_eq!(from_bip155(3, &hex("f1f2f3f4f5f6f7f8f9fa"), 1).unwrap(), None);
        assert_eq!(from_bip155(0xaa, &[0; 600], 1).unwrap(), None);
        assert_eq!(from_bip155(bip155::IPV6, &hex("00000000000000000000ffff01020304"), 1).unwrap(), None);
        assert_eq!(from_bip155(bip155::IPV6, &hex("fd87d87eeb430102030405060708090a"), 1).unwrap(), None);
        assert_eq!(from_bip155(bip155::CJDNS, &hex("aa000001000200030004000500060007"), 1).unwrap(), None);
    }

    #[test]
    fn text_round_trip() {
        for s in [
            "1.2.3.4:9999",
            "[2a01:4f8::1]:19999",
            "[fc00:1:2:3:4:5:6:7]:9999",
            &format!("{TORV3_NAME}:9999"),
            &format!("{I2P_NAME}:0"),
        ] {
            let a: PeerAddr = s.parse().unwrap();
            assert_eq!(a.to_string(), s);
        }
        // Case-insensitive input, canonical lowercase output.
        let up: PeerAddr = format!("{}:9999", TORV3_NAME.to_uppercase()).parse().unwrap();
        assert_eq!(up.to_string(), format!("{TORV3_NAME}:9999"));
        // IPv4-mapped IPv6 is IPv4.
        assert_eq!("[::ffff:1.2.3.4]:9999".parse::<PeerAddr>().unwrap().to_string(), "1.2.3.4:9999");
    }

    #[test]
    fn rejects_bad_names() {
        // TORv2 (16 chars), bad checksum, wrong version, short I2P, non-zero trailing bits.
        assert!("6hzph5hv6337r6p2.onion:9999".parse::<PeerAddr>().is_err());
        let mut bad = TORV3_NAME.to_string();
        bad.replace_range(0..1, "q");
        assert!(format!("{bad}:9999").parse::<PeerAddr>().is_err());
        assert!("aaaa.b32.i2p:0".parse::<PeerAddr>().is_err());
        let mut i2p = I2P_NAME.to_string();
        i2p.replace_range(51..52, "r"); // 'q' = 16 -> trailing 4 bits 0000; 'r' = 17 -> 0001
        assert!(format!("{i2p}:0").parse::<PeerAddr>().is_err());
        assert!("example.com:9999".parse::<PeerAddr>().is_err());
        assert!("1.2.3.4".parse::<PeerAddr>().is_err());
    }

    #[test]
    fn base32_round_trip() {
        for len in 0..40 {
            let data: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(base32_decode(&base32_encode(&data)).unwrap(), data);
        }
        assert_eq!(base32_encode(b"foobar"), "mzxw6ytboi"); // RFC 4648 test vector, unpadded
    }

    #[test]
    fn routability_per_network() {
        for (a, ok) in [
            ("8.8.8.8:9999", true),
            ("10.0.0.1:9999", false),
            ("192.168.1.1:9999", false),
            ("100.64.1.1:9999", false),
            ("127.0.0.1:9999", false),
            ("8.8.8.8:0", false),
            ("[2a01:4f8::1]:9999", true),
            ("[fd87:d87e:eb43::1]:9999", false), // ULA, not CJDNS
            ("[fe80::1]:9999", false),
            ("[2001:db8::1]:9999", false),
            ("[fc00:1:2:3:4:5:6:7]:9999", true), // CJDNS, not ULA garbage
            ("[fc00:1:2:3:4:5:6:7]:0", false),
            (&format!("{TORV3_NAME}:9999"), true),
            (&format!("{TORV3_NAME}:0"), false),
            (&format!("{I2P_NAME}:0"), true),
            (&format!("{I2P_NAME}:9999"), false),
        ] {
            assert_eq!(a.parse::<PeerAddr>().unwrap().is_routable(), ok, "{a}");
        }
    }

    #[test]
    fn classification() {
        assert_eq!(net_of("1.2.3.4:9999"), Some(AddrNet::Ipv4));
        assert_eq!(net_of("[2a01::1]:9999"), Some(AddrNet::Ipv6));
        assert_eq!(net_of("[fd00::1]:9999"), Some(AddrNet::Ipv6));
        assert_eq!(net_of("[fc12::1]:9999"), Some(AddrNet::Cjdns));
        assert_eq!(net_of(&format!("{TORV3_NAME}:9999")), Some(AddrNet::Onion));
        assert_eq!(net_of(&format!("{I2P_NAME}:0")), Some(AddrNet::I2p));
        assert_eq!(net_of("garbage"), None);
        // getnodeaddresses form: bare host plus network name.
        let a = PeerAddr::from_parts("fc00:1:2:3:4:5:6:7", 9999).unwrap();
        assert_eq!(a.net(), AddrNet::Cjdns);
        let a = PeerAddr::from_parts("2a01:4f8::1", 9999).unwrap();
        assert_eq!(a.to_string(), "[2a01:4f8::1]:9999");
        let a = PeerAddr::from_parts(I2P_NAME, 0).unwrap();
        assert_eq!(a.net(), AddrNet::I2p);
        assert!(PeerAddr::from_parts("nonsense", 1).is_none());
    }
}

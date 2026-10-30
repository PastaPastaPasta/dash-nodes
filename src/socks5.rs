//! Minimal SOCKS5 client (RFC 1928): no authentication, CONNECT only, which is all Tor's SocksPort,
//! i2pd's SOCKS proxy and a generic IPv6 proxy need. Written here rather than pulled in as a crate
//! because it's small, and because replies come from a proxy relaying for untrusted peers: every
//! field is checked and nothing is read beyond the reply.

use anyhow::{Result, anyhow, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::addr::SocksTarget;

const VERSION: u8 = 5;
const NO_AUTH: u8 = 0;
const CMD_CONNECT: u8 = 1;
const ATYP_IPV4: u8 = 1;
const ATYP_DOMAIN: u8 = 3;
const ATYP_IPV6: u8 = 4;

/// Runs the SOCKS5 greeting and CONNECT on an open connection to the proxy. On success the stream
/// is connected to `target`. The caller bounds the time this takes.
pub async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, target: &SocksTarget) -> Result<()> {
    s.write_all(&[VERSION, 1, NO_AUTH]).await?;
    let mut choice = [0u8; 2];
    s.read_exact(&mut choice).await.map_err(|e| anyhow!("socks: read: {}", e.kind()))?;
    if choice[0] != VERSION {
        bail!("socks: not a SOCKS5 proxy");
    }
    if choice[1] != NO_AUTH {
        bail!("socks: proxy requires authentication");
    }
    s.write_all(&connect_request(target)?).await?;

    let mut head = [0u8; 4];
    s.read_exact(&mut head).await.map_err(|e| anyhow!("socks: read: {}", e.kind()))?;
    let [ver, rep, rsv, atyp] = head;
    if ver != VERSION || rsv != 0 {
        bail!("socks: malformed reply");
    }
    if rep != 0 {
        bail!("socks: {}", reply_message(rep));
    }
    // The bound address is meaningless to us but must be consumed: the peer's bytes follow it.
    let addr_len = match atyp {
        ATYP_IPV4 => 4,
        ATYP_IPV6 => 16,
        ATYP_DOMAIN => s.read_u8().await.map_err(|e| anyhow!("socks: read: {}", e.kind()))? as usize,
        _ => bail!("socks: malformed reply (address type {atyp})"),
    };
    let mut rest = [0u8; 255 + 2];
    s.read_exact(&mut rest[..addr_len + 2]).await.map_err(|e| anyhow!("socks: read: {}", e.kind()))?;
    Ok(())
}

fn connect_request(target: &SocksTarget) -> Result<Vec<u8>> {
    let mut req = vec![VERSION, CMD_CONNECT, 0];
    let port = match target {
        SocksTarget::Ip(sa) => {
            match sa.ip() {
                std::net::IpAddr::V4(ip) => {
                    req.push(ATYP_IPV4);
                    req.extend_from_slice(&ip.octets());
                }
                std::net::IpAddr::V6(ip) => {
                    req.push(ATYP_IPV6);
                    req.extend_from_slice(&ip.octets());
                }
            }
            sa.port()
        }
        SocksTarget::Domain(name, port) => {
            if name.is_empty() || name.len() > 255 {
                bail!("socks: bad host name length {}", name.len());
            }
            req.push(ATYP_DOMAIN);
            req.push(name.len() as u8);
            req.extend_from_slice(name.as_bytes());
            *port
        }
    };
    req.extend_from_slice(&port.to_be_bytes());
    Ok(req)
}

/// RFC 1928 reply codes, plus Tor's onion-service codes (sent with the SocksPort `ExtendedErrors` flag).
fn reply_message(rep: u8) -> String {
    match rep {
        1 => "general failure",
        2 => "not allowed by ruleset",
        3 => "network unreachable",
        4 => "host unreachable",
        5 => "connection refused",
        6 => "TTL expired",
        7 => "command not supported",
        8 => "address type not supported",
        0xf0 => "onion service descriptor not found",
        0xf1 => "onion service descriptor invalid",
        0xf2 => "onion service introduction failed",
        0xf3 => "onion service rendezvous failed",
        0xf4 => "onion service needs client authorization",
        0xf5 => "onion service client authorization wrong",
        0xf6 => "invalid onion address",
        0xf7 => "onion service introduction timed out",
        _ => return format!("reply code {rep:#04x}"),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::io::duplex;

    /// Plays the proxy side: checks the greeting and request, then sends `reply` (and `trailer`,
    /// standing in for the peer's first bytes).
    async fn run(target: SocksTarget, method: u8, reply: Vec<u8>, trailer: &[u8]) -> (Result<()>, Vec<u8>, Vec<u8>) {
        let (mut client, mut server) = duplex(1024);
        let trailer = trailer.to_vec();
        let proxy = tokio::spawn(async move {
            let mut greet = [0u8; 3];
            server.read_exact(&mut greet).await.unwrap();
            assert_eq!(greet, [5, 1, 0]);
            server.write_all(&[5, method]).await.unwrap();
            if method != 0 {
                return Vec::new();
            }
            let mut req = vec![0u8; 512];
            let n = server.read(&mut req).await.unwrap();
            req.truncate(n);
            server.write_all(&reply).await.unwrap();
            server.write_all(&trailer).await.unwrap();
            req
        });
        let result = handshake(&mut client, &target).await;
        let req = proxy.await.unwrap();
        let mut after = vec![0u8; 0];
        if result.is_ok() {
            let mut buf = [0u8; 3];
            if client.read_exact(&mut buf).await.is_ok() {
                after = buf.to_vec();
            }
        }
        (result, req, after)
    }

    #[tokio::test]
    async fn connects_by_domain_and_leaves_peer_bytes() {
        let target = SocksTarget::Domain("abc.onion".into(), 9999);
        // Reply with a domain-type bound address (Tor uses IPv4 0.0.0.0; any type must parse).
        let reply = [&[5u8, 0, 0, 3, 4][..], b"bind", &[0x27, 0x0f]].concat();
        let (r, req, after) = run(target, 0, reply, b"\xbf\x0c\x6b").await;
        r.unwrap();
        assert_eq!(req, [&[5u8, 1, 0, 3, 9][..], b"abc.onion", &[0x27, 0x0f]].concat());
        assert_eq!(after, b"\xbf\x0c\x6b"); // the stream is positioned at the peer's data
    }

    #[tokio::test]
    async fn connects_by_ipv6() {
        let sa: SocketAddr = "[fc00::1]:9999".parse().unwrap();
        let reply = [&[5u8, 0, 0, 4][..], &[0u8; 16], &[0, 0]].concat();
        let (r, req, _) = run(SocksTarget::Ip(sa), 0, reply, b"").await;
        r.unwrap();
        assert_eq!(req[..4], [5, 1, 0, 4]);
        assert_eq!(req.len(), 4 + 16 + 2);
    }

    #[tokio::test]
    async fn reports_errors() {
        let t = || SocksTarget::Domain("x.onion".into(), 9999);
        let (r, _, _) = run(t(), 0, vec![5, 4, 0, 1, 0, 0, 0, 0, 0, 0], b"").await;
        assert_eq!(r.unwrap_err().to_string(), "socks: host unreachable");
        let (r, _, _) = run(t(), 0, vec![5, 0xf0, 0, 1, 0, 0, 0, 0, 0, 0], b"").await;
        assert_eq!(r.unwrap_err().to_string(), "socks: onion service descriptor not found");
        let (r, _, _) = run(t(), 0xff, vec![], b"").await;
        assert_eq!(r.unwrap_err().to_string(), "socks: proxy requires authentication");
        // Malformed: wrong version, non-zero reserved byte, unknown address type, truncated.
        for reply in [vec![4, 0, 0, 1], vec![5, 0, 1, 1], vec![5, 0, 0, 9, 0, 0], vec![5, 0, 0, 1, 1, 2]] {
            let (r, _, _) = run(t(), 0, reply.clone(), b"").await;
            assert!(r.is_err(), "{reply:?}");
        }
    }

    #[test]
    fn rejects_oversized_names() {
        assert!(connect_request(&SocksTarget::Domain("a".repeat(256), 1)).is_err());
        assert!(connect_request(&SocksTarget::Domain(String::new(), 1)).is_err());
    }
}

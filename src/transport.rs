//! How each address network is reached: directly, through a SOCKS5 proxy, or not at all. Routes
//! are re-evaluated at the start of every crawl, so a proxy or CJDNS router that comes up later
//! is picked up without a restart.

use std::net::{IpAddr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::addr::{AddrNet, PeerAddr, is_cjdns_ip};
use crate::p2p::Timeouts;
use crate::socks5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Direct,
    /// SOCKS5 proxy, as `host:port`.
    Socks5(String),
}

/// On / off / detect at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Toggle {
    Auto,
    On,
    Off,
}

impl std::str::FromStr for Toggle {
    type Err = String;

    fn from_str(s: &str) -> Result<Toggle, String> {
        match s.to_ascii_lowercase().as_str() {
            "auto" | "" => Ok(Toggle::Auto),
            "true" | "1" | "yes" | "on" => Ok(Toggle::On),
            "false" | "0" | "no" | "off" => Ok(Toggle::Off),
            _ => Err(format!("{s}: expected auto, true or false")),
        }
    }
}

#[derive(Clone, Debug)]
pub struct TransportConfig {
    pub ipv6: Toggle,
    pub ipv6_proxy: Option<String>,
    pub tor_proxy: Option<String>,
    pub i2p_proxy: Option<String>,
    pub cjdns: Toggle,
}

/// Whether and how a network is crawled right now; `reason` says why not when it isn't.
#[derive(Clone, Debug)]
pub struct NetRoute {
    pub route: Option<Route>,
    pub reason: Option<String>,
}

impl NetRoute {
    fn via(route: Route) -> NetRoute {
        NetRoute { route: Some(route), reason: None }
    }

    fn off(reason: &str) -> NetRoute {
        NetRoute { route: None, reason: Some(reason.into()) }
    }
}

/// One `NetRoute` per network, indexed by `AddrNet::index`.
#[derive(Clone, Debug)]
pub struct Routes([NetRoute; AddrNet::COUNT]);

impl Routes {
    pub fn get(&self, net: AddrNet) -> &NetRoute {
        &self.0[net.index()]
    }

    /// The route to probe `addr` over, if it's routable and its network is reachable.
    pub fn for_addr(&self, addr: &PeerAddr) -> Option<&Route> {
        if !addr.is_routable() {
            return None;
        }
        self.get(addr.net()).route.as_ref()
    }
}

impl TransportConfig {
    pub fn routes(&self) -> Routes {
        let socks = |p: &Option<String>| p.as_ref().filter(|p| !p.is_empty()).map(|p| Route::Socks5(p.clone()));
        let ipv6 = match (self.ipv6, socks(&self.ipv6_proxy)) {
            (Toggle::Off, _) => NetRoute::off("disabled (CRAWL_IPV6=false)"),
            (_, Some(proxy)) => NetRoute::via(proxy),
            (Toggle::On, None) => NetRoute::via(Route::Direct),
            (Toggle::Auto, None) if has_global_ipv6_route() => NetRoute::via(Route::Direct),
            (Toggle::Auto, None) => NetRoute::off("no IPv6 route"),
        };
        let onion = socks(&self.tor_proxy).map_or_else(|| NetRoute::off("no Tor proxy (TOR_PROXY)"), NetRoute::via);
        let i2p = socks(&self.i2p_proxy).map_or_else(|| NetRoute::off("no I2P proxy (I2P_PROXY)"), NetRoute::via);
        let cjdns = match self.cjdns {
            Toggle::Off => NetRoute::off("disabled (CJDNS=false)"),
            Toggle::On => NetRoute::via(Route::Direct),
            Toggle::Auto if has_cjdns_route() => NetRoute::via(Route::Direct),
            Toggle::Auto => NetRoute::off("no CJDNS route (fc00::/8)"),
        };
        Routes([NetRoute::via(Route::Direct), ipv6, onion, i2p, cjdns])
    }
}

/// Source address the kernel would pick to reach `dst`, without sending anything. None when
/// there's no route (or no IPv6 at all).
fn source_for(dst: Ipv6Addr) -> Option<Ipv6Addr> {
    let sock = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0)).ok()?;
    sock.connect(SocketAddr::new(IpAddr::V6(dst), 53)).ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V6(ip) => Some(ip),
        IpAddr::V4(_) => None,
    }
}

/// A route to the IPv6 internet from a global address. A default route from a ULA address (e.g.
/// a Docker network with NAT66 and no upstream IPv6) doesn't count: it usually leads nowhere, and
/// CRAWL_IPV6=true covers the cases where it does.
fn has_global_ipv6_route() -> bool {
    // 2001:4860:4860::8888, Google Public DNS: any well-known global address works.
    source_for(Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888))
        .is_some_and(|src| (src.segments()[0] & 0xe000) == 0x2000)
}

/// A local CJDNS address (fc00::/8) that the kernel would use for fc00::/8 destinations, i.e. a
/// cjdns TUN interface in this network namespace. Routes through a NAT (CJDNS=true) can't be
/// detected this way.
fn has_cjdns_route() -> bool {
    source_for(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 1)).is_some_and(|src| is_cjdns_ip(&src))
}

/// Opens a TCP stream to `addr` over `route` within `limit` (for proxies: the proxy's own connect
/// to the target, which for Tor and I2P includes building the circuit or tunnel).
pub async fn connect(route: &Route, addr: &PeerAddr, limit: Duration) -> Result<TcpStream> {
    let attempt = async {
        match route {
            Route::Direct => {
                let Some(sa) = addr.socket_addr() else { bail!("{} needs a proxy", addr.net()) };
                TcpStream::connect(sa).await.map_err(|e| anyhow!("connect: {}", e.kind()))
            }
            Route::Socks5(proxy) => {
                let mut s =
                    TcpStream::connect(proxy.as_str()).await.map_err(|e| anyhow!("proxy connect: {}", e.kind()))?;
                socks5::handshake(&mut s, &addr.socks_target()).await?;
                Ok(s)
            }
        }
    };
    let stream = timeout(limit, attempt).await.map_err(|_| anyhow!("connect timeout"))??;
    stream.set_nodelay(true).ok();
    Ok(stream)
}

/// Per-network crawl limits: Tor and I2P are slow to connect and shouldn't be flooded.
#[derive(Clone, Debug)]
pub struct NetLimits {
    pub timeouts: Timeouts,
    pub concurrency: usize,
}

pub fn default_limits(
    direct_concurrency: usize,
    tor_concurrency: usize,
    i2p_concurrency: usize,
) -> [NetLimits; AddrNet::COUNT] {
    let t = |connect: u64, handshake: u64, getaddr: u64| Timeouts {
        connect: Duration::from_secs(connect),
        handshake: Duration::from_secs(handshake),
        getaddr: Duration::from_secs(getaddr),
    };
    [
        NetLimits { timeouts: t(5, 10, 10), concurrency: direct_concurrency },
        NetLimits { timeouts: t(10, 10, 10), concurrency: direct_concurrency },
        // Onion services: descriptor fetch, introduction and rendezvous, often 5-20 s.
        NetLimits { timeouts: t(60, 30, 30), concurrency: tor_concurrency },
        // I2P: LeaseSet lookup and tunnel build; 30 s and more is normal.
        NetLimits { timeouts: t(90, 60, 60), concurrency: i2p_concurrency },
        // CJDNS: route discovery across the mesh adds seconds to the first packet.
        NetLimits { timeouts: t(20, 15, 15), concurrency: direct_concurrency.min(32) },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(ipv6: Toggle, cjdns: Toggle) -> TransportConfig {
        TransportConfig { ipv6, ipv6_proxy: None, tor_proxy: None, i2p_proxy: None, cjdns }
    }

    #[test]
    fn routes_follow_config() {
        let r = cfg(Toggle::Off, Toggle::Off).routes();
        assert_eq!(r.get(AddrNet::Ipv4).route, Some(Route::Direct));
        assert!(r.get(AddrNet::Ipv6).route.is_none());
        assert_eq!(r.get(AddrNet::Onion).reason.as_deref(), Some("no Tor proxy (TOR_PROXY)"));
        assert!(r.get(AddrNet::I2p).route.is_none());
        assert_eq!(r.get(AddrNet::Cjdns).reason.as_deref(), Some("disabled (CJDNS=false)"));

        let mut c = cfg(Toggle::Auto, Toggle::On);
        c.tor_proxy = Some("172.30.86.20:9050".into());
        c.i2p_proxy = Some(String::new()); // empty env var = unset
        c.ipv6_proxy = Some("10.0.0.1:1080".into());
        let r = c.routes();
        assert_eq!(r.get(AddrNet::Onion).route, Some(Route::Socks5("172.30.86.20:9050".into())));
        assert!(r.get(AddrNet::I2p).route.is_none());
        assert_eq!(r.get(AddrNet::Ipv6).route, Some(Route::Socks5("10.0.0.1:1080".into())));
        assert_eq!(r.get(AddrNet::Cjdns).route, Some(Route::Direct));
        // Off wins over a configured proxy.
        c.ipv6 = Toggle::Off;
        assert!(c.routes().get(AddrNet::Ipv6).route.is_none());
    }

    #[test]
    fn unroutable_addresses_get_no_route() {
        let r = cfg(Toggle::On, Toggle::On).routes();
        assert!(r.for_addr(&"8.8.8.8:9999".parse().unwrap()).is_some());
        assert!(r.for_addr(&"10.1.1.1:9999".parse().unwrap()).is_none());
        assert!(r.for_addr(&"[fd00::1]:9999".parse().unwrap()).is_none());
        assert!(r.for_addr(&"[fc00::1]:9999".parse().unwrap()).is_some());
    }

    #[test]
    fn toggles_parse() {
        assert_eq!("auto".parse::<Toggle>().unwrap(), Toggle::Auto);
        assert_eq!("TRUE".parse::<Toggle>().unwrap(), Toggle::On);
        assert_eq!("0".parse::<Toggle>().unwrap(), Toggle::Off);
        assert!("maybe".parse::<Toggle>().is_err());
    }

    #[tokio::test]
    async fn direct_route_refuses_overlay_addresses() {
        let onion: PeerAddr = "pg6mmjiyjmcrsslvykfwnntlaru7p5svn6y2ymmju6nubxndf4pscryd.onion:9999".parse().unwrap();
        let e = connect(&Route::Direct, &onion, Duration::from_secs(1)).await.unwrap_err();
        assert_eq!(e.to_string(), "onion needs a proxy");
    }
}

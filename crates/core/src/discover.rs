//! Finding an Xbox on the local network.
//!
//! Xbox dashboards do not announce themselves, so this probes every address in each local /24
//! for an FTP server and reads its greeting. Dashboards name themselves in it (UnleashX,
//! EvolutionX, Avalaunch, XBMC), which is how a console is told apart from a NAS or a printer.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

const XBOX_HINTS: &[&str] = &[
    "xbox", "unleashx", "evox", "evolution", "avalaunch", "xbmc", "cerbios", "nexgen", "xbox media center",
];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Found {
    pub host: String,
    pub port: u16,
    pub banner: String,
    /// The greeting names an Xbox dashboard.
    pub looks_like_xbox: bool,
}

/// A network searched, named by this machine's address on it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Network {
    /// The adapter, e.g. `Wi-Fi`, `en0` or `vEthernet (WSL)`.
    pub adapter: String,
    pub address: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Discovery {
    pub networks: Vec<Network>,
    pub found: Vec<Found>,
}

/// The address of the interface this machine would route LAN traffic through. Sends nothing:
/// connecting a UDP socket only selects a route.
pub fn primary_ipv4() -> Option<Ipv4Addr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    match s.local_addr().ok()?.ip() {
        IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => Some(v4),
        _ => None,
    }
}

/// Every private IPv4 address on an adapter that is up, one per /24, the routed one first.
/// Windows machines often have several (Wi-Fi, Ethernet, Hyper-V and WSL switches, VPNs), and
/// the console may be on any of them, e.g. plugged into the Ethernet port while Wi-Fi has the
/// internet.
pub fn local_networks() -> Vec<Network> {
    let primary = primary_ipv4();
    let mut v4: Vec<(String, Ipv4Addr)> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| i.is_oper_up() && !i.is_loopback())
        .filter_map(|i| match i.ip() {
            IpAddr::V4(ip) if ip.is_private() => Some((i.name, ip)),
            _ => None,
        })
        .collect();
    if let Some(p) = primary {
        if !v4.iter().any(|(_, ip)| *ip == p) {
            v4.push((String::new(), p));
        }
    }
    v4.sort_by_key(|(_, ip)| Some(*ip) != primary);
    let mut seen = Vec::new();
    v4.into_iter()
        .filter(|(_, ip)| {
            let net = slash24(*ip);
            !seen.contains(&net) && {
                seen.push(net);
                true
            }
        })
        .map(|(adapter, ip)| Network { adapter, address: ip.to_string() })
        .collect()
}

fn slash24(ip: Ipv4Addr) -> [u8; 3] {
    let [a, b, c, _] = ip.octets();
    [a, b, c]
}

pub fn looks_like_xbox(banner: &str) -> bool {
    let b = banner.to_ascii_lowercase();
    XBOX_HINTS.iter().any(|h| b.contains(h))
}

async fn probe(addr: SocketAddr, timeout: Duration) -> Option<Found> {
    let mut s = tokio::time::timeout(timeout, TcpStream::connect(addr)).await.ok()?.ok()?;
    // A greeting may run to many lines (UnleashX sends a dozen of drive statistics first);
    // read until its final `220 ` line, which is the one that names the server.
    let mut buf = vec![0u8; 4096];
    let mut got = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    while got < buf.len() {
        let n = match tokio::time::timeout_at(deadline, s.read(&mut buf[got..])).await {
            Ok(Ok(n)) => n,
            _ => break,
        };
        if n == 0 {
            break;
        }
        got += n;
        let text = String::from_utf8_lossy(&buf[..got]);
        if text.lines().any(|l| l.starts_with("220 ")) && text.ends_with('\n') {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf[..got]).into_owned();
    if !text.starts_with("220") {
        return None;
    }
    let banner = crate::ftp::server_name(&text);
    Some(Found {
        host: addr.ip().to_string(),
        port: addr.port(),
        looks_like_xbox: looks_like_xbox(&banner),
        banner,
    })
}

/// Probes every host in the /24 containing `near`, or by default in each of this machine's
/// networks (`local_networks`), for an FTP server on `port`. Calls `on_found` as servers
/// answer; returns them all, Xbox-looking ones first.
pub async fn discover<F>(near: Option<Ipv4Addr>, port: u16, on_found: F) -> Discovery
where
    F: Fn(&Found) + Send + Sync + 'static,
{
    let networks = match near {
        Some(ip) => vec![Network { adapter: String::new(), address: ip.to_string() }],
        None => local_networks(),
    };
    let on_found = Arc::new(on_found);
    let limit = Arc::new(Semaphore::new(128));
    let mut set = JoinSet::new();
    for net in &networks {
        let Ok(ip) = net.address.parse::<Ipv4Addr>() else { continue };
        let [a, b, c] = slash24(ip);
        for d in 1..=254u8 {
            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(a, b, c, d)), port);
            let limit = limit.clone();
            let on_found = on_found.clone();
            set.spawn(async move {
                let _permit = limit.acquire_owned().await.ok()?;
                let f = probe(addr, Duration::from_millis(1000)).await?;
                on_found(&f);
                Some(f)
            });
        }
    }
    let mut found = Vec::new();
    while let Some(r) = set.join_next().await {
        if let Ok(Some(f)) = r {
            found.push(f);
        }
    }
    found.sort_by(|x, y| {
        y.looks_like_xbox
            .cmp(&x.looks_like_xbox)
            .then_with(|| x.host.parse::<Ipv4Addr>().ok().cmp(&y.host.parse::<Ipv4Addr>().ok()))
    });
    Discovery { networks, found }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_dashboards() {
        assert!(looks_like_xbox("220 UnleashX FTP Server ready"));
        assert!(looks_like_xbox("220 Welcome to XBMC"));
        assert!(!looks_like_xbox("220 ProFTPD Server (Debian)"));
    }
}

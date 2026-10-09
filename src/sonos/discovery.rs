//! Finding one speaker is enough: it reports the whole household's topology.
//!
//! SSDP replies are unicast UDP that host firewalls (ufw) usually drop, so
//! when multicast discovery comes back empty we sweep the local /24 for port
//! 1400 instead, which only needs outbound TCP.

use anyhow::{anyhow, Result};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::timeout;

use super::soap::Soap;

async fn is_sonos(soap: &Soap, ip: &str) -> bool {
    let url = format!("http://{ip}:1400/xml/device_description.xml");
    match soap.http().get(url).timeout(Duration::from_secs(2)).send().await {
        Ok(r) => r.text().await.map(|t| t.contains("Sonos")).unwrap_or(false),
        Err(_) => false,
    }
}

async fn ssdp() -> Option<String> {
    let sock = UdpSocket::bind("0.0.0.0:0").await.ok()?;
    let msg = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: urn:schemas-upnp-org:device:ZonePlayer:1\r\n\r\n";
    sock.send_to(msg.as_bytes(), "239.255.255.250:1900").await.ok()?;
    let mut buf = [0u8; 2048];
    let (_, from) = timeout(Duration::from_millis(1500), sock.recv_from(&mut buf)).await.ok()?.ok()?;
    Some(from.ip().to_string())
}

fn local_ipv4() -> Option<Ipv4Addr> {
    // Connecting a UDP socket sends nothing; it just picks the outgoing interface.
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    match s.local_addr().ok()?.ip() {
        IpAddr::V4(v4) => Some(v4),
        _ => None,
    }
}

async fn sweep(soap: &Soap) -> Option<String> {
    let me = local_ipv4()?;
    let [a, b, c, _] = me.octets();
    let mut tasks = Vec::new();
    for d in 1..=254u8 {
        let ip = Ipv4Addr::new(a, b, c, d);
        if ip == me {
            continue;
        }
        tasks.push(tokio::spawn(async move {
            let addr = SocketAddr::new(IpAddr::V4(ip), 1400);
            timeout(Duration::from_millis(700), TcpStream::connect(addr)).await.ok()?.ok()?;
            Some(ip.to_string())
        }));
    }
    for t in tasks {
        if let Ok(Some(ip)) = t.await {
            if is_sonos(soap, &ip).await {
                return Some(ip);
            }
        }
    }
    None
}

/// Returns the IP of any reachable speaker, trying cached ones first.
pub async fn find_speaker(soap: &Soap, known: &[String]) -> Result<String> {
    for ip in known {
        if is_sonos(soap, ip).await {
            return Ok(ip.clone());
        }
    }
    if let Some(ip) = ssdp().await {
        if is_sonos(soap, &ip).await {
            return Ok(ip);
        }
    }
    sweep(soap).await.ok_or_else(|| anyhow!("No Sonos speakers found on this network"))
}

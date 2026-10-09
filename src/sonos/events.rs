//! UPnP eventing (GENA): instead of asking every second, subscribe once and
//! the speakers call us back the moment a track, volume, queue or grouping
//! changes. Events here are just wake-ups; the UI re-reads what changed with
//! the normal calls, so there is one parser for everything.
//!
//! Callbacks are plain HTTP NOTIFY requests to a port on this PC, so a
//! firewall must let the LAN reach it. Until the first callback arrives the
//! app keeps polling; `heard()` says when it's safe to slow down.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

pub const EVENT_PORT: u16 = 8900;
/// Asked-for subscription length; renewed at half of what the speaker grants.
const TIMEOUT_S: u64 = 1800;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Track, play state, play mode.
    Transport,
    /// The group's volume and mute.
    GroupVolume,
    /// Queue edits (and other content changes).
    Queue,
    /// Rooms joining or leaving groups.
    Topology,
}

impl Kind {
    fn path(self) -> &'static str {
        match self {
            Kind::Transport => "/MediaRenderer/AVTransport/Event",
            Kind::GroupVolume => "/MediaRenderer/GroupRenderingControl/Event",
            Kind::Queue => "/MediaServer/ContentDirectory/Event",
            Kind::Topology => "/ZoneGroupTopology/Event",
        }
    }
    fn slug(self) -> &'static str {
        match self {
            Kind::Transport => "transport",
            Kind::GroupVolume => "volume",
            Kind::Queue => "queue",
            Kind::Topology => "topology",
        }
    }
    fn from_slug(s: &str) -> Option<Kind> {
        [Kind::Transport, Kind::GroupVolume, Kind::Queue, Kind::Topology].into_iter().find(|k| k.slug() == s)
    }
}

#[derive(Debug, Clone)]
pub struct Event {
    pub kind: Kind,
    /// The speaker that sent it.
    pub ip: String,
}

struct Sub {
    sid: String,
    renew: JoinHandle<()>,
}

struct Inner {
    http: reqwest::Client,
    callback_host: String,
    subs: Mutex<HashMap<(String, Kind), Sub>>,
    heard: AtomicBool,
}

#[derive(Clone)]
pub struct Events {
    inner: Arc<Inner>,
}

impl Events {
    /// Listens for callbacks and forwards each as an [`Event`].
    pub async fn start(http: reqwest::Client, tx: UnboundedSender<Event>) -> Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", EVENT_PORT)).await.with_context(|| format!("port {EVENT_PORT} is busy"))?;
        let ip = crate::audio::lan_ip()?;
        let inner = Arc::new(Inner { http, callback_host: format!("{ip}:{EVENT_PORT}"), subs: Mutex::default(), heard: AtomicBool::new(false) });
        let me = Self { inner };
        let w = Arc::downgrade(&me.inner);
        tokio::spawn(async move {
            while let Ok((mut sock, peer)) = listener.accept().await {
                let Some(inner) = w.upgrade() else { return };
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Some(kind) = read_notify(&mut sock).await {
                        inner.heard.store(true, Ordering::Relaxed);
                        let _ = tx.send(Event { kind, ip: peer.ip().to_string() });
                    }
                    let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                });
            }
        });
        Ok(me)
    }

    /// True once any speaker has managed to call back.
    pub fn heard(&self) -> bool {
        self.inner.heard.load(Ordering::Relaxed)
    }

    /// Makes the subscriptions exactly `wanted`: new ones are added, ones no
    /// longer wanted are cancelled, and existing ones are left alone.
    pub async fn set(&self, wanted: &[(String, Kind)]) {
        let stale: Vec<((String, Kind), Sub)> = {
            let mut subs = self.inner.subs.lock().unwrap();
            let keys: Vec<_> = subs.keys().filter(|k| !wanted.contains(k)).cloned().collect();
            keys.into_iter().filter_map(|k| subs.remove(&k).map(|s| (k, s))).collect()
        };
        for ((ip, kind), sub) in stale {
            sub.renew.abort();
            let _ = self.unsubscribe(&ip, kind, &sub.sid).await;
        }
        for (ip, kind) in wanted {
            if self.inner.subs.lock().unwrap().contains_key(&(ip.clone(), *kind)) {
                continue;
            }
            if let Err(e) = self.subscribe(ip.clone(), *kind).await {
                eprintln!("event subscription {ip} {kind:?}: {e:#}");
            }
        }
    }

    async fn subscribe(&self, ip: String, kind: Kind) -> Result<()> {
        let url = format!("http://{ip}:1400{}", kind.path());
        let r = self
            .inner
            .http
            .request(reqwest::Method::from_bytes(b"SUBSCRIBE")?, &url)
            .header("CALLBACK", format!("<http://{}/{}>", self.inner.callback_host, kind.slug()))
            .header("NT", "upnp:event")
            .header("TIMEOUT", format!("Second-{TIMEOUT_S}"))
            .send()
            .await?;
        if !r.status().is_success() {
            return Err(anyhow!("SUBSCRIBE {}", r.status()));
        }
        let sid = r.headers().get("SID").and_then(|v| v.to_str().ok()).ok_or_else(|| anyhow!("no SID"))?.to_string();
        let granted = granted_secs(&r);
        let renew = tokio::spawn(renew_loop(Arc::downgrade(&self.inner), ip.clone(), kind, sid.clone(), granted));
        self.inner.subs.lock().unwrap().insert((ip, kind), Sub { sid, renew });
        Ok(())
    }

    async fn unsubscribe(&self, ip: &str, kind: Kind, sid: &str) -> Result<()> {
        let url = format!("http://{ip}:1400{}", kind.path());
        self.inner.http.request(reqwest::Method::from_bytes(b"UNSUBSCRIBE")?, &url).header("SID", sid).send().await?;
        Ok(())
    }

    /// Cancels everything, e.g. on quit, so speakers stop calling a dead port.
    pub async fn shutdown(&self) {
        self.set(&[]).await;
    }
}

fn granted_secs(r: &reqwest::Response) -> u64 {
    r.headers()
        .get("TIMEOUT")
        .and_then(|v| v.to_str().ok())
        .and_then(|t| t.trim().strip_prefix("Second-"))
        .and_then(|n| n.parse().ok())
        .unwrap_or(TIMEOUT_S)
}

async fn renew_loop(inner: std::sync::Weak<Inner>, ip: String, kind: Kind, mut sid: String, mut secs: u64) {
    loop {
        tokio::time::sleep(Duration::from_secs((secs / 2).max(30))).await;
        let Some(i) = inner.upgrade() else { return };
        let url = format!("http://{ip}:1400{}", kind.path());
        let Ok(method) = reqwest::Method::from_bytes(b"SUBSCRIBE") else { return };
        let renewed = i.http.request(method, &url).header("SID", &sid).header("TIMEOUT", format!("Second-{TIMEOUT_S}")).send().await;
        match renewed {
            Ok(r) if r.status().is_success() => secs = granted_secs(&r),
            // The speaker forgot us (rebooted, or the subscription lapsed): start over.
            _ => {
                let Ok(method) = reqwest::Method::from_bytes(b"SUBSCRIBE") else { return };
                let fresh = i
                    .http
                    .request(method, &url)
                    .header("CALLBACK", format!("<http://{}/{}>", i.callback_host, kind.slug()))
                    .header("NT", "upnp:event")
                    .header("TIMEOUT", format!("Second-{TIMEOUT_S}"))
                    .send()
                    .await;
                match fresh {
                    Ok(r) if r.status().is_success() => {
                        if let Some(s) = r.headers().get("SID").and_then(|v| v.to_str().ok()) {
                            sid = s.to_string();
                            if let Some(sub) = i.subs.lock().unwrap().get_mut(&(ip.clone(), kind)) {
                                sub.sid = sid.clone();
                            }
                        }
                        secs = granted_secs(&r);
                    }
                    _ => secs = 120,
                }
            }
        }
    }
}

/// Reads one NOTIFY request; returns which subscription it belongs to.
async fn read_notify(sock: &mut tokio::net::TcpStream) -> Option<Kind> {
    let mut buf = Vec::with_capacity(16 * 1024);
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        let n = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut chunk)).await.ok()?.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    if first.next()? != "NOTIFY" {
        return None;
    }
    let kind = Kind::from_slug(first.next()?.trim_start_matches('/'))?;
    // Drain the body so the speaker sees a clean exchange.
    let len: usize = lines.find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.trim().parse().ok())).unwrap_or(0);
    let mut have = buf.len() - head_end;
    while have < len {
        match tokio::time::timeout(Duration::from_secs(5), sock.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => have += n,
            _ => break,
        }
    }
    Some(kind)
}

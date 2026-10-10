//! Your phone as the calibration microphone: a small HTTPS page on this PC
//! that records with the phone's mic and streams the take back.
//!
//! Browsers only allow the microphone on secure pages, so this serves HTTPS
//! with a self-signed certificate (kept in the config folder so the phone
//! only has to accept it once). A random token in the QR code's link keeps
//! other devices on the network out.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::rustls;

pub const PORT: u16 = 8901;
const PAGE: &str = include_str!("phone.html");
/// A take longer than this (10 minutes at 48 kHz) is cut off.
const MAX_SAMPLES: usize = 48_000 * 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Waiting,
    Measuring,
    Done,
}

#[derive(Default)]
struct Take {
    rate: u32,
    samples: Vec<f32>,
    /// The phone's own sample count where this take begins.
    base: Option<usize>,
    /// Earliest bound on the PC instant of sample 0.
    origin: Option<Instant>,
    last_chunk: Option<Instant>,
}

struct Inner {
    token: String,
    take: Mutex<Take>,
    phase: Mutex<(Phase, String)>,
}

#[derive(Clone)]
pub struct Phone {
    inner: Arc<Inner>,
    pub url: String,
    task: Arc<tokio::task::AbortHandle>,
}

fn cert_paths() -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = dirs::config_dir().unwrap_or_else(|| ".".into()).join("sonance");
    (dir.join("phone-cert.pem"), dir.join("phone-key.pem"))
}

/// The page's certificate, made once and reused, so the phone remembers it.
fn certificate(ip: &str) -> Result<(String, String)> {
    let (cp, kp) = cert_paths();
    if let (Ok(c), Ok(k)) = (std::fs::read_to_string(&cp), std::fs::read_to_string(&kp)) {
        // A new LAN address needs a new certificate.
        if std::fs::read_to_string(cp.with_extension("ip")).is_ok_and(|old| old == ip) {
            return Ok((c, k));
        }
    }
    let ck = rcgen::generate_simple_self_signed(vec![ip.to_string(), "sonance.local".into()])?;
    let (c, k) = (ck.cert.pem(), ck.signing_key.serialize_pem());
    if let Some(dir) = cp.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&cp, &c)?;
    std::fs::write(&kp, &k)?;
    std::fs::write(cp.with_extension("ip"), ip)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&kp, std::fs::Permissions::from_mode(0o600))?;
    Ok((c, k))
}

fn tls_config(ip: &str) -> Result<rustls::ServerConfig> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
    let (c, k) = certificate(ip)?;
    let certs = CertificateDer::pem_slice_iter(c.as_bytes()).collect::<Result<Vec<_>, _>>()?;
    let key = PrivateKeyDer::from_pem_slice(k.as_bytes())?;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    Ok(rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)?)
}

impl Phone {
    /// Starts the page; `url` is what the QR code should hold.
    pub async fn start() -> Result<Self> {
        let ip = crate::audio::lan_ip()?.to_string();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config(&ip)?));
        let listener = TcpListener::bind(("0.0.0.0", PORT)).await.with_context(|| format!("port {PORT} is busy"))?;
        let mut raw = [0u8; 12];
        std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut raw))?;
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
        let inner = Arc::new(Inner { token: token.clone(), take: Mutex::default(), phase: Mutex::new((Phase::Waiting, String::new())) });
        let w = Arc::downgrade(&inner);
        let task = tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let Some(inner) = w.upgrade() else { return };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if let Ok(Ok(tls)) = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(sock)).await {
                        let _ = serve(tls, &inner).await;
                    }
                });
            }
        });
        Ok(Self { inner, url: format!("https://{ip}:{PORT}/?t={token}"), task: Arc::new(task.abort_handle()) })
    }

    pub fn stop(&self) {
        self.task.abort();
    }

    /// True while the phone is recording and sending.
    pub fn connected(&self) -> bool {
        self.inner.take.lock().unwrap().last_chunk.is_some_and(|t| t.elapsed() < Duration::from_secs(3))
    }

    pub fn set_phase(&self, phase: Phase, message: &str) {
        *self.inner.phase.lock().unwrap() = (phase, message.to_string());
    }

    /// Drops what was recorded so far; the next sample is the take's start.
    pub fn restart_take(&self) {
        let mut t = self.inner.take.lock().unwrap();
        let rate = t.rate;
        *t = Take { rate, last_chunk: t.last_chunk, ..Default::default() };
    }

    /// The take so far: (rate, samples, PC instant of sample 0).
    pub fn take(&self) -> Option<(u32, Vec<f32>, Instant)> {
        let t = self.inner.take.lock().unwrap();
        Some((t.rate, t.samples.clone(), t.origin?)).filter(|(r, s, _)| *r > 0 && !s.is_empty())
    }
}

/// One request per connection: GET / (the page), GET /state, POST /chunk.
async fn serve<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(mut sock: S, inner: &Inner) -> Result<()> {
    let mut buf = Vec::with_capacity(64 * 1024);
    let mut chunk = [0u8; 16384];
    let head_end = loop {
        let n = tokio::time::timeout(Duration::from_secs(10), sock.read(&mut chunk)).await??;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 32 * 1024 {
            return Ok(());
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or("").split_whitespace();
    let (method, target) = (first.next().unwrap_or(""), first.next().unwrap_or("/"));
    let len: usize = lines
        .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.trim().parse().ok()))
        .unwrap_or(0)
        .min(8 * 1024 * 1024);
    while buf.len() - head_end < len {
        let n = tokio::time::timeout(Duration::from_secs(10), sock.read(&mut chunk)).await??;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = &buf[head_end..(head_end + len).min(buf.len())];
    let received = Instant::now();

    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let param = |k: &str| query.split('&').find_map(|kv| kv.strip_prefix(k).and_then(|v| v.strip_prefix('='))).unwrap_or("");
    let authorised = param("t") == inner.token;
    let (status, ctype, out): (&str, &str, Vec<u8>) = match (method, path) {
        _ if !authorised => ("403 Forbidden", "text/plain", b"Scan the QR code in Sonance again.".to_vec()),
        ("GET", "/") => ("200 OK", "text/html; charset=utf-8", PAGE.as_bytes().to_vec()),
        ("GET", "/state") => {
            let (phase, msg) = inner.phase.lock().unwrap().clone();
            let phase = match phase {
                Phase::Waiting => "waiting",
                Phase::Measuring => "measuring",
                Phase::Done => "done",
            };
            ("200 OK", "application/json", serde_json::json!({ "phase": phase, "message": msg }).to_string().into_bytes())
        }
        ("POST", "/chunk") => {
            let offset: usize = param("offset").parse().unwrap_or(0);
            let rate: u32 = param("rate").parse().unwrap_or(0);
            let samples: Vec<f32> = body.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
            add_chunk(&mut inner.take.lock().unwrap(), offset, rate, &samples, received);
            ("204 No Content", "text/plain", Vec::new())
        }
        _ => ("404 Not Found", "text/plain", b"not found".to_vec()),
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        out.len()
    );
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(&out).await?;
    sock.shutdown().await?;
    Ok(())
}

/// Chunks carry their position in the phone's own count of samples; the take
/// starts at the first chunk after a restart. Each arrival bounds when the
/// take's first sample was recorded: no later than (arrival − samples so far).
fn add_chunk(t: &mut Take, offset: usize, rate: u32, samples: &[f32], received: Instant) {
    if rate == 0 || samples.is_empty() {
        return;
    }
    if t.rate != rate {
        *t = Take { rate, ..Default::default() };
    }
    t.last_chunk = Some(received);
    let base = *t.base.get_or_insert(offset);
    let Some(pos) = offset.checked_sub(base) else { return };
    if pos < t.samples.len() || pos + samples.len() > MAX_SAMPLES {
        return; // a repeat, or far too long
    }
    // A lost chunk: keep the timeline by filling the gap with silence.
    t.samples.resize(pos, 0.0);
    t.samples.extend_from_slice(samples);
    let start = received.checked_sub(Duration::from_secs_f64(t.samples.len() as f64 / rate as f64)).unwrap_or(received);
    t.origin = Some(t.origin.map_or(start, |o| o.min(start)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_build_a_timeline() {
        let mut t = Take::default();
        let now = Instant::now();
        add_chunk(&mut t, 4800, 48_000, &[0.1; 4800], now);
        add_chunk(&mut t, 9600, 48_000, &[0.2; 4800], now + Duration::from_millis(130));
        assert_eq!(t.samples.len(), 9600);
        // Origin: the tighter of (now − 0.1 s) and (now + 0.13 − 0.2 s).
        let o = t.origin.unwrap();
        assert!((now - o).as_secs_f32() - 0.1 < 0.001);
        // A repeat is ignored.
        add_chunk(&mut t, 9600, 48_000, &[0.3; 4800], now + Duration::from_millis(200));
        assert_eq!(t.samples.len(), 9600);
    }
}

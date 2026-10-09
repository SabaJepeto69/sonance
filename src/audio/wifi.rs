//! Wi-Fi mode: the `sonance` monitor, encoded and served over HTTP for Sonos to pull.
//!
//! Each format has one capture + encoder running for as long as the stream is on, fanned out
//! to every HTTP client through a broadcast channel. A client that can't keep up lags out of
//! the channel and is disconnected rather than slowing anyone else down.

use std::net::{Ipv4Addr, UdpSocket};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::task::{JoinHandle, JoinSet};

use super::{Engine, SINK, pw};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamUrls {
    pub mp3: String,
    pub flac: String,
    pub wav: Option<String>,
}

const RATE: u32 = 48_000;
/// Chunks a client may fall behind by (a few seconds of audio) before it is dropped.
const BACKLOG: usize = 512;

/// The LAN IPv4 address of this PC: whichever one the kernel would route to the internet from.
/// Connecting a UDP socket sends nothing; it only selects the route.
pub fn lan_ip() -> Result<Ipv4Addr> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    sock.connect((Ipv4Addr::new(1, 1, 1, 1), 80))?;
    match sock.local_addr()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_unspecified() => Ok(ip),
        ip => anyhow::bail!("no usable LAN address ({ip})"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Mp3,
    Flac,
    Wav,
}

impl Format {
    const ALL: [Format; 3] = [Format::Mp3, Format::Flac, Format::Wav];

    fn ext(self) -> &'static str {
        match self {
            Format::Mp3 => "mp3",
            Format::Flac => "flac",
            Format::Wav => "wav",
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            Format::Mp3 => "audio/mpeg",
            Format::Flac => "audio/flac",
            Format::Wav => "audio/wav",
        }
    }

    /// ffmpeg output options; WAV needs no encoder, the PCM is served as captured.
    fn encoder(self) -> Option<&'static [&'static str]> {
        match self {
            Format::Mp3 => Some(&["-c:a", "libmp3lame", "-b:a", "320k", "-id3v2_version", "0", "-write_xing", "0", "-f", "mp3"]),
            Format::Flac => Some(&["-c:a", "flac", "-compression_level", "0", "-frame_size", "1152", "-f", "flac"]),
            Format::Wav => None,
        }
    }

    /// Where a late joiner can start decoding mid-stream.
    fn sync(self, b: &[u8]) -> Option<usize> {
        match self {
            Format::Mp3 => b.windows(2).position(|w| w[0] == 0xFF && w[1] & 0xE0 == 0xE0),
            Format::Flac => b.windows(2).position(|w| w[0] == 0xFF && w[1] & 0xFE == 0xF8),
            Format::Wav => Some(0),
        }
    }
}

/// A RIFF/WAVE header claiming the longest possible data chunk, for an endless PCM stream.
pub fn wav_header(rate: u32, channels: u16, bits: u16) -> Vec<u8> {
    let align = channels * bits / 8;
    let data = (u32::MAX - 36) / align as u32 * align as u32;
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(data + 36).to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes());
    h.extend_from_slice(&channels.to_le_bytes());
    h.extend_from_slice(&rate.to_le_bytes());
    h.extend_from_slice(&(rate * align as u32).to_le_bytes());
    h.extend_from_slice(&align.to_le_bytes());
    h.extend_from_slice(&bits.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&data.to_le_bytes());
    h
}

/// Length of the `fLaC` marker plus all metadata blocks, once `b` holds all of them.
pub fn flac_header_len(b: &[u8]) -> Option<usize> {
    if b.len() < 4 || &b[..4] != b"fLaC" {
        return None;
    }
    let mut at = 4;
    loop {
        let h = b.get(at..at + 4)?;
        let len = u32::from_be_bytes([0, h[1], h[2], h[3]]) as usize;
        at += 4 + len;
        if h[0] & 0x80 != 0 {
            return (at <= b.len()).then_some(at);
        }
    }
}

type Chunk = Arc<Vec<u8>>;

/// What a new client needs: the live channel and the bytes that must precede any audio.
#[derive(Default)]
struct Feed {
    tx: Option<broadcast::Sender<Chunk>>,
    header: Option<Vec<u8>>,
}

type SharedFeed = Arc<Mutex<Feed>>;

pub(super) struct Server {
    port: u16,
    urls: StreamUrls,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// One capture (pw-cat on the sink monitor) piped into one encoder, until either dies.
async fn encode_once(fmt: Format, feed: &SharedFeed) -> Result<()> {
    let mut cap = pw::command("pw-cat")
        .args(["--record", "--raw", "--rate", "48000", "--channels", "2", "--format", "s16", "--latency", "10ms"])
        .args(["--target", SINK, "-P"])
        .arg(format!(
            "{{ node.name=sonance-wifi-{} node.description=\"Sonance Wi-Fi ({})\" stream.capture.sink=true \
             node.dont-fallback=true node.dont-reconnect=true state.restore-target=false }}",
            fmt.ext(),
            fmt.ext()
        ))
        .arg("-")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("couldn't start pw-cat")?;
    let pcm = cap.stdout.take().context("capture stdout")?;
    let mut enc = None;
    let mut out: Box<dyn AsyncRead + Unpin + Send> = match fmt.encoder() {
        None => Box::new(pcm),
        Some(opts) => {
            let pcm: Stdio = pcm.try_into()?;
            let mut child = pw::command("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-fflags", "nobuffer", "-flags", "low_delay"])
                .args(["-probesize", "32", "-analyzeduration", "0"])
                .args(["-f", "s16le", "-ar", "48000", "-ac", "2", "-i", "pipe:0"])
                .args(opts)
                .args(["-flush_packets", "1", "pipe:1"])
                .stdin(pcm)
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .context("couldn't start ffmpeg")?;
            let stdout = child.stdout.take().context("ffmpeg stdout")?;
            enc = Some(child);
            Box::new(stdout)
        }
    };
    let (tx, _) = broadcast::channel(BACKLOG);
    {
        let mut f = feed.lock().unwrap();
        f.tx = Some(tx.clone());
        f.header = match fmt {
            Format::Wav => Some(wav_header(RATE, 2, 16)),
            Format::Mp3 => Some(Vec::new()),
            Format::Flac => None,
        };
    }
    let mut buf = vec![0u8; 8192];
    let mut pending = Vec::new();
    let mut have_header = fmt != Format::Flac;
    let result = loop {
        let n = match out.read(&mut buf).await {
            Ok(0) => break Ok(()),
            Ok(n) => n,
            Err(e) => break Err(e.into()),
        };
        pending.extend_from_slice(&buf[..n]);
        if !have_header {
            let Some(len) = flac_header_len(&pending) else { continue };
            feed.lock().unwrap().header = Some(pending.drain(..len).collect());
            have_header = true;
        }
        // PCM goes out in whole frames so a late joiner starts on a sample boundary.
        let take = if fmt == Format::Wav { pending.len() / 4 * 4 } else { pending.len() };
        if take > 0 {
            let _ = tx.send(Arc::new(pending.drain(..take).collect()));
        }
    };
    *feed.lock().unwrap() = Feed::default();
    drop(enc);
    drop(cap);
    result
}

async fn run_encoder(fmt: Format, feed: SharedFeed) {
    loop {
        if let Err(e) = encode_once(fmt, &feed).await {
            eprintln!("sonance: {} stream: {e:#}", fmt.ext());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Reads the request head; returns (method, path) without the query string.
async fn read_request(sock: &mut TcpStream) -> Option<(String, String)> {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = sock.read(&mut buf).await.ok()?;
        if n == 0 || head.len() > 16 * 1024 {
            return None;
        }
        head.extend_from_slice(&buf[..n]);
    }
    let line = String::from_utf8_lossy(&head);
    let mut parts = line.lines().next()?.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.split('?').next()?.to_string();
    Some((method, path))
}

async fn serve_client(mut sock: TcpStream, feeds: Arc<Vec<(Format, SharedFeed)>>) {
    let _ = sock.set_nodelay(true);
    let Ok(Some((method, path))) = tokio::time::timeout(Duration::from_secs(10), read_request(&mut sock)).await else {
        return;
    };
    let found = feeds.iter().find(|(f, _)| path == format!("/sonance.{}", f.ext()));
    let reply = |status: &str| format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let Some((fmt, feed)) = found else {
        let _ = sock.write_all(reply("404 Not Found").as_bytes()).await;
        return;
    };
    if method != "GET" && method != "HEAD" {
        let _ = sock.write_all(reply("405 Method Not Allowed").as_bytes()).await;
        return;
    }
    let mut head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nConnection: close\r\nCache-Control: no-cache, no-store\r\n\
         Accept-Ranges: none\r\nServer: Sonance\r\n",
        fmt.content_type()
    );
    if *fmt == Format::Mp3 {
        head.push_str("icy-name: Sonance\r\n");
    }
    head.push_str("\r\n");
    if method == "HEAD" {
        let _ = sock.write_all(head.as_bytes()).await;
        return;
    }
    // The encoder may still be starting (or restarting): give it a moment.
    let mut joined = None;
    for _ in 0..60 {
        {
            let f = feed.lock().unwrap();
            if let (Some(tx), Some(h)) = (&f.tx, &f.header) {
                joined = Some((tx.subscribe(), h.clone()));
            }
        }
        if joined.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let Some((mut rx, header)) = joined else {
        let _ = sock.write_all(reply("503 Service Unavailable").as_bytes()).await;
        return;
    };
    if sock.write_all(head.as_bytes()).await.is_err() || sock.write_all(&header).await.is_err() {
        return;
    }
    let mut synced = false;
    let write_timeout = Duration::from_secs(10);
    while let Ok(chunk) = rx.recv().await {
        let mut data = &chunk[..];
        if !synced {
            let Some(at) = fmt.sync(data) else { continue };
            data = &data[at..];
            synced = true;
        }
        match tokio::time::timeout(write_timeout, sock.write_all(data)).await {
            Ok(Ok(())) => {}
            _ => return,
        }
    }
}

async fn accept_loop(listener: TcpListener, feeds: Arc<Vec<(Format, SharedFeed)>>) {
    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            r = listener.accept() => match r {
                Ok((sock, _)) => {
                    clients.spawn(serve_client(sock, feeds.clone()));
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            },
            Some(_) = clients.join_next(), if !clients.is_empty() => {}
        }
    }
}

impl Engine {
    /// Starts capturing the Sonance output and serving it on `port`; returns the URLs served.
    /// Calling it again with the same port just returns the running stream's URLs.
    pub async fn start_wifi_stream(&self, port: u16) -> Result<StreamUrls> {
        self.ensure_sink().await?;
        let mut st = self.inner.state.lock().await;
        if let Some(s) = &st.wifi
            && s.port == port
        {
            return Ok(s.urls.clone());
        }
        st.wifi = None;
        let ip = lan_ip()?;
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))
            .await
            .with_context(|| format!("couldn't listen on port {port}"))?;
        let feeds: Arc<Vec<(Format, SharedFeed)>> = Arc::new(Format::ALL.iter().map(|&f| (f, SharedFeed::default())).collect());
        let mut tasks: Vec<JoinHandle<()>> = feeds.iter().map(|(f, feed)| tokio::spawn(run_encoder(*f, feed.clone()))).collect();
        tasks.push(tokio::spawn(accept_loop(listener, feeds)));
        let url = |f: Format| format!("http://{ip}:{port}/sonance.{}", f.ext());
        let urls = StreamUrls { mp3: url(Format::Mp3), flac: url(Format::Flac), wav: Some(url(Format::Wav)) };
        st.wifi = Some(Server { port, urls: urls.clone(), tasks });
        Ok(urls)
    }

    pub async fn stop_wifi_stream(&self) {
        let server = self.inner.state.lock().await.wifi.take();
        if let Some(mut s) = server {
            let tasks = std::mem::take(&mut s.tasks);
            for t in &tasks {
                t.abort();
            }
            // Awaiting the aborted tasks drops their children, which kills them.
            for t in tasks {
                let _ = t.await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_layout() {
        let h = wav_header(48_000, 2, 16);
        assert_eq!(h.len(), 44);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(&h[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes([h[22], h[23]]), 2);
        assert_eq!(u32::from_le_bytes(h[24..28].try_into().unwrap()), 48_000);
        assert_eq!(u32::from_le_bytes(h[28..32].try_into().unwrap()), 192_000);
        assert_eq!(u16::from_le_bytes([h[32], h[33]]), 4);
        let data = u32::from_le_bytes(h[40..44].try_into().unwrap());
        assert_eq!(data % 4, 0);
        assert_eq!(u32::from_le_bytes(h[4..8].try_into().unwrap()), data + 36);
    }

    #[test]
    fn flac_header() {
        let mut b = b"fLaC".to_vec();
        b.extend_from_slice(&[0x00, 0, 0, 34]);
        b.extend_from_slice(&[0; 34]);
        assert_eq!(flac_header_len(&b), None, "needs the last-block flag");
        b.extend_from_slice(&[0x84, 0, 0, 8]);
        b.extend_from_slice(&[0; 7]);
        assert_eq!(flac_header_len(&b), None, "incomplete last block");
        b.extend_from_slice(&[0, 0xFF, 0xF8]);
        assert_eq!(flac_header_len(&b), Some(4 + 38 + 12));
        assert_eq!(Format::Flac.sync(&b[53..]), Some(1));
    }
}

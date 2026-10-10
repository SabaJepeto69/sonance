//! Spotify Web API, used only for searching and listing your library.
//! Playback goes through Sonos's own linked Spotify account, so this needs
//! no Spotify Connect and keeps the Sonos queue in charge.
//!
//! Auth is PKCE (no client secret) with a loopback redirect, which is what
//! Spotify requires of desktop apps.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::config::{Config, Tokens};

pub const REDIRECT_PORT: u16 = 8898;
pub fn redirect_uri() -> String {
    format!("http://127.0.0.1:{REDIRECT_PORT}/callback")
}
/// `user-read-playback-state` lets "play this PC's Spotify on the speakers" bring the
/// playlist or album along, not just the song; older sign-ins lack it and get the song only.
const SCOPES: &str = "user-library-read playlist-read-private playlist-read-collaborative user-read-private user-read-playback-state";

#[derive(Clone, Debug)]
pub struct SpItem {
    /// spotify:track:..., spotify:album:..., spotify:playlist:..., spotify:artist:...
    pub uri: String,
    pub title: String,
    pub subtitle: String,
    pub art: Option<String>,
}

impl SpItem {
    pub fn kind(&self) -> &str {
        self.uri.split(':').nth(1).unwrap_or("")
    }
}

pub struct PendingLogin {
    pub url: String,
    verifier: String,
    state: String,
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn random(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)).expect("/dev/urandom");
    buf
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn begin_login(client_id: &str) -> PendingLogin {
    let verifier = b64(&random(48));
    let challenge = b64(&Sha256::digest(verifier.as_bytes()));
    let state = b64(&random(12));
    let url = format!(
        "https://accounts.spotify.com/authorize?client_id={}&response_type=code&redirect_uri={}&code_challenge_method=S256&code_challenge={}&state={}&scope={}",
        urlencoding::encode(client_id),
        urlencoding::encode(&redirect_uri()),
        challenge,
        state,
        urlencoding::encode(SCOPES)
    );
    PendingLogin { url, verifier, state }
}

#[derive(Clone)]
pub struct Spotify {
    http: reqwest::Client,
    cfg: Arc<Mutex<Config>>,
    /// Refresh tokens are single-use, so parallel requests must not refresh twice.
    refreshing: Arc<tokio::sync::Mutex<()>>,
    /// The loopback listener of a login in progress, so a retry can free the port.
    login: Arc<Mutex<Option<tokio::task::AbortHandle>>>,
}

impl Spotify {
    pub fn new(http: reqwest::Client, cfg: Arc<Mutex<Config>>) -> Self {
        Self { http, cfg, refreshing: Arc::default(), login: Arc::default() }
    }

    pub fn logged_in(&self) -> bool {
        self.cfg.lock().unwrap().spotify.is_some()
    }

    pub fn logout(&self) {
        let mut c = self.cfg.lock().unwrap();
        c.spotify = None;
        c.save();
    }

    fn store(&self, v: &Value, old_refresh: Option<String>) -> Result<()> {
        let access_token = v["access_token"].as_str().ok_or_else(|| anyhow!("Spotify: {v}"))?.to_string();
        let refresh_token = v["refresh_token"].as_str().map(String::from).or(old_refresh).unwrap_or_default();
        let expires_at = now() + v["expires_in"].as_u64().unwrap_or(3600);
        let mut c = self.cfg.lock().unwrap();
        c.spotify = Some(Tokens { access_token, refresh_token, expires_at });
        c.save();
        Ok(())
    }

    /// Waits for the browser to hit the loopback redirect, then trades the code for tokens.
    /// Spawn this before opening `pending.url`.
    pub async fn finish_login(&self, client_id: String, pending: PendingLogin) -> Result<()> {
        let me = self.clone();
        let task = tokio::spawn(async move { me.login_flow(client_id, pending).await });
        if let Some(old) = self.login.lock().unwrap().replace(task.abort_handle()) {
            old.abort();
        }
        match task.await {
            Ok(r) => r,
            Err(e) if e.is_cancelled() => bail!("Replaced by a newer login"),
            Err(e) => bail!("Login failed: {e}"),
        }
    }

    async fn login_flow(&self, client_id: String, pending: PendingLogin) -> Result<()> {
        let listener = match TcpListener::bind(("127.0.0.1", REDIRECT_PORT)).await {
            Ok(l) => l,
            Err(_) => {
                // An aborted earlier login may still be releasing the port.
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                TcpListener::bind(("127.0.0.1", REDIRECT_PORT)).await.with_context(|| format!("port {REDIRECT_PORT} is busy"))?
            }
        };
        let code = loop {
            let (mut sock, _) = listener.accept().await?;
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let target = req.split_whitespace().nth(1).unwrap_or("");
            let Some(query) = target.strip_prefix("/callback?") else {
                let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
                continue;
            };
            let param = |k: &str| {
                query.split('&').find_map(|kv| kv.strip_prefix(&format!("{k}="))).map(|v| urlencoding::decode(v).map(|c| c.into_owned()).unwrap_or_default())
            };
            let (msg, result) = match (param("code"), param("error")) {
                _ if param("state").as_deref() != Some(pending.state.as_str()) => ("Login failed: state mismatch.", Err(anyhow!("state mismatch"))),
                (Some(code), _) => ("Spotify is connected. You can close this tab and go back to Sonance.", Ok(code)),
                (_, err) => ("Spotify login was cancelled.", Err(anyhow!("Spotify login: {}", err.unwrap_or_default()))),
            };
            let html = format!("<html><body style='font-family:sans-serif;padding:3em'><h2>{msg}</h2></body></html>");
            let _ = sock
                .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}", html.len()).as_bytes())
                .await;
            break result?;
        };
        let v: Value = self
            .http
            .post("https://accounts.spotify.com/api/token")
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", &redirect_uri()),
                ("client_id", &client_id),
                ("code_verifier", &pending.verifier),
            ])
            .send()
            .await?
            .json()
            .await?;
        self.store(&v, None)
    }

    fn stored(&self) -> Result<(Tokens, String)> {
        let c = self.cfg.lock().unwrap();
        Ok((c.spotify.clone().ok_or_else(|| anyhow!("Not signed in to Spotify"))?, c.spotify_client_id.clone()))
    }

    /// `stale` is an access token the API just rejected, forcing a refresh.
    async fn token(&self, stale: Option<&str>) -> Result<String> {
        let (tok, _) = self.stored()?;
        let fresh = |t: &Tokens| t.expires_at > now() + 60 && Some(t.access_token.as_str()) != stale;
        if fresh(&tok) {
            return Ok(tok.access_token);
        }
        let _guard = self.refreshing.lock().await;
        // Another request may have refreshed while we waited.
        let (tok, client_id) = self.stored()?;
        if fresh(&tok) {
            return Ok(tok.access_token);
        }
        let v: Value = self
            .http
            .post("https://accounts.spotify.com/api/token")
            .form(&[("grant_type", "refresh_token"), ("refresh_token", &tok.refresh_token), ("client_id", &client_id)])
            .send()
            .await?
            .json()
            .await?;
        if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
            // Only a rejected refresh token (revoked, or past its lifetime) means signing in again;
            // anything else (server error, rate limit) is worth a retry later.
            if err == "invalid_grant" {
                self.logout();
                bail!("Spotify session expired, please sign in again");
            }
            bail!("Spotify token refresh failed: {err}");
        }
        self.store(&v, Some(tok.refresh_token))?;
        Ok(self.cfg.lock().unwrap().spotify.as_ref().map(|t| t.access_token.clone()).unwrap_or_default())
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        let mut stale: Option<String> = None;
        loop {
            let tok = self.token(stale.as_deref()).await?;
            let r = self.http.get(format!("https://api.spotify.com/v1{path}")).query(query).bearer_auth(&tok).send().await?;
            let status = r.status();
            if status.as_u16() == 401 && stale.is_none() {
                stale = Some(tok);
                continue;
            }
            let v: Value = r.json().await.unwrap_or(Value::Null);
            if !status.is_success() {
                let msg = v["error"]["message"].as_str().unwrap_or("request failed");
                bail!("Spotify {}: {msg}", status.as_u16());
            }
            return Ok(v);
        }
    }

    pub async fn search(&self, q: &str) -> Result<Vec<SpItem>> {
        // Development-mode apps are capped at 10 results per type.
        let v = self.get("/search", &[("q", q), ("type", "track,artist,album,playlist"), ("limit", "10")]).await?;
        let mut out = Vec::new();
        for key in ["tracks", "artists", "albums", "playlists"] {
            if let Some(items) = v[key]["items"].as_array() {
                out.extend(items.iter().filter_map(parse_item));
            }
        }
        Ok(out)
    }

    pub async fn artist_albums(&self, name: &str) -> Result<Vec<SpItem>> {
        let q = format!("artist:\"{name}\"");
        let v = self.get("/search", &[("q", &q), ("type", "album"), ("limit", "10")]).await?;
        Ok(v["albums"]["items"].as_array().map(|a| a.iter().filter_map(parse_item).collect()).unwrap_or_default())
    }

    pub async fn my_playlists(&self) -> Result<Vec<SpItem>> {
        let v = self.get("/me/playlists", &[("limit", "50")]).await?;
        Ok(v["items"].as_array().map(|a| a.iter().filter_map(parse_item).collect()).unwrap_or_default())
    }

    /// The album or playlist the account is playing from, if any.
    pub async fn playing_context(&self) -> Result<Option<String>> {
        let v = self.get("/me/player", &[]).await?;
        let uri = v["context"]["uri"].as_str().unwrap_or("");
        Ok(["spotify:album:", "spotify:playlist:"].iter().any(|p| uri.starts_with(p)).then(|| uri.to_string()))
    }

    /// 0-based position of `track` in an album or playlist.
    pub async fn index_in(&self, context: &str, track: &str) -> Result<Option<u32>> {
        let mut parts = context.split(':').skip(1);
        let (kind, id) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let (path, page) = match kind {
            "album" => (format!("/albums/{id}/tracks"), 50),
            "playlist" => (format!("/playlists/{id}/items"), 100),
            _ => return Ok(None),
        };
        let mut offset = 0u32;
        while offset < 5000 {
            let v = self.get(&path, &[("limit", &page.to_string()), ("offset", &offset.to_string())]).await?;
            let items = v["items"].as_array().cloned().unwrap_or_default();
            for (i, it) in items.iter().enumerate() {
                // Album tracks are the items; playlist entries wrap them (as "item", formerly "track").
                let uri = it["uri"].as_str().or(it["item"]["uri"].as_str()).or(it["track"]["uri"].as_str());
                if uri == Some(track) {
                    return Ok(Some(offset + i as u32));
                }
            }
            if items.len() < page || v["next"].is_null() {
                break;
            }
            offset += page as u32;
        }
        Ok(None)
    }

    pub async fn my_albums(&self) -> Result<Vec<SpItem>> {
        let v = self.get("/me/albums", &[("limit", "50")]).await?;
        Ok(v["items"].as_array().map(|a| a.iter().filter_map(|i| parse_item(&i["album"])).collect()).unwrap_or_default())
    }
}

fn names(v: &Value) -> String {
    v.as_array().map(|a| a.iter().filter_map(|x| x["name"].as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default()
}

fn image(v: &Value) -> Option<String> {
    // Images come largest first; the middle one (~300px) is plenty.
    let imgs = v.as_array()?;
    imgs.get(1).or(imgs.first())?["url"].as_str().map(String::from)
}

fn parse_item(v: &Value) -> Option<SpItem> {
    let uri = v["uri"].as_str()?.to_string();
    let title = v["name"].as_str().unwrap_or("").to_string();
    let kind = uri.split(':').nth(1).unwrap_or("");
    let (subtitle, art) = match kind {
        "track" => (format!("{} · {}", names(&v["artists"]), v["album"]["name"].as_str().unwrap_or("")), image(&v["album"]["images"])),
        "album" => (format!("Album · {}", names(&v["artists"])), image(&v["images"])),
        "playlist" => (format!("Playlist · {}", v["owner"]["display_name"].as_str().unwrap_or("")), image(&v["images"])),
        "artist" => ("Artist".to_string(), image(&v["images"])),
        _ => return None,
    };
    Some(SpItem { uri, title, subtitle, art })
}

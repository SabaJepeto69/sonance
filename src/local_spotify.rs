//! The Spotify app running on this PC, seen through its MPRIS interface:
//! what it's playing and where, so Sonance can hand it over to the speakers.

use std::collections::HashMap;

use anyhow::{Context, Result};
use tokio::sync::OnceCell;
use zbus::zvariant::OwnedValue;

const NAME: &str = "org.mpris.MediaPlayer2.spotify";
const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";

#[derive(Debug, Clone, PartialEq)]
pub struct Local {
    /// spotify:track:<id>
    pub uri: String,
    pub title: String,
    pub artist: String,
    pub position_ms: u64,
    pub playing: bool,
}

static CONN: OnceCell<zbus::Connection> = OnceCell::const_new();

async fn conn() -> Result<&'static zbus::Connection> {
    CONN.get_or_try_init(|| async { zbus::Connection::session().await.context("no session bus") }).await
}

async fn get(c: &zbus::Connection, prop: &str) -> Result<OwnedValue> {
    let reply = c.call_method(Some(NAME), PATH, Some("org.freedesktop.DBus.Properties"), "Get", &(PLAYER, prop)).await?;
    let v: OwnedValue = reply.body().deserialize()?;
    Ok(v)
}

/// The track id from Spotify's metadata: `mpris:trackid` is `/com/spotify/track/<id>` or
/// `spotify:track:<id>` depending on the version; `xesam:url` is the web link.
fn track_uri(md: &HashMap<String, OwnedValue>) -> Option<String> {
    let s = |k: &str| md.get(k).and_then(|v| String::try_from(v.try_clone().ok()?).ok().or_else(|| v.downcast_ref::<zbus::zvariant::ObjectPath>().ok().map(|p| p.to_string())));
    for cand in [s("mpris:trackid"), s("xesam:url")].into_iter().flatten() {
        let id = cand
            .strip_prefix("/com/spotify/track/")
            .or_else(|| cand.strip_prefix("spotify:track:"))
            .or_else(|| cand.strip_prefix("https://open.spotify.com/track/"))
            .map(|id| id.split(['?', '/']).next().unwrap_or(id));
        if let Some(id) = id.filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric())) {
            return Some(format!("spotify:track:{id}"));
        }
    }
    None
}

/// What the Spotify app plays, or `None` if it isn't running or plays nothing (or an ad).
pub async fn now_playing() -> Option<Local> {
    let c = conn().await.ok()?;
    let status: String = get(c, "PlaybackStatus").await.ok()?.try_into().ok()?;
    let md: HashMap<String, OwnedValue> = get(c, "Metadata").await.ok()?.try_into().ok()?;
    let uri = track_uri(&md)?;
    let text = |k: &str| md.get(k).and_then(|v| String::try_from(v.try_clone().ok()?).ok()).unwrap_or_default();
    let artist = md.get("xesam:artist").and_then(|v| Vec::<String>::try_from(v.try_clone().ok()?).ok()).map(|a| a.join(", ")).unwrap_or_default();
    let position_us: i64 = get(c, "Position").await.ok().and_then(|v| v.try_into().ok()).unwrap_or(0);
    Some(Local { uri, title: text("xesam:title"), artist, position_ms: (position_us.max(0) / 1000) as u64, playing: status == "Playing" })
}

pub async fn pause() -> Result<()> {
    conn().await?.call_method(Some(NAME), PATH, Some(PLAYER), "Pause", &()).await?;
    Ok(())
}

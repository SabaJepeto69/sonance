//! MPRIS: the D-Bus interface desktop shells use for "what's playing".
//! Publishing it gives Sonance media keys, GNOME's top-bar media controls and
//! lock-screen controls, which keep working with the window closed.
//!
//! The D-Bus side only reads a shared snapshot and forwards commands; the
//! GTK side owns all real state and acts on the commands.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

const PATH: &str = "/org/mpris/MediaPlayer2";

#[derive(Debug, Clone)]
pub enum Cmd {
    Raise,
    Quit,
    PlayPause,
    Play,
    Pause,
    Stop,
    Next,
    Previous,
    /// Relative, microseconds.
    Seek(i64),
    /// Absolute, microseconds.
    SetPosition(i64),
    /// 0.0 – 1.0.
    Volume(f64),
    Shuffle(bool),
    /// "None", "Track" or "Playlist".
    Loop(String),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    pub playing: bool,
    pub paused: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub art: Option<String>,
    pub track_no: u32,
    pub length_us: i64,
    pub position_us: i64,
    pub volume: f64,
    pub shuffle: bool,
    /// "None", "Track" or "Playlist".
    pub loop_status: String,
    pub can_skip: bool,
}

type Shared = Arc<Mutex<State>>;

struct Root {
    tx: UnboundedSender<Cmd>,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    fn raise(&self) {
        let _ = self.tx.send(Cmd::Raise);
    }
    fn quit(&self) {
        let _ = self.tx.send(Cmd::Quit);
    }
    #[zbus(property)]
    fn can_quit(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_raise(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn identity(&self) -> String {
        "Sonance".into()
    }
    #[zbus(property)]
    fn desktop_entry(&self) -> String {
        "dev.sonance.Sonance".into()
    }
    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }
    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

struct Player {
    tx: UnboundedSender<Cmd>,
    state: Shared,
}

impl Player {
    fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
    }
    fn get(&self) -> State {
        self.state.lock().unwrap().clone()
    }
}

fn track_path(n: u32) -> ObjectPath<'static> {
    ObjectPath::try_from(format!("/dev/sonance/track/{n}")).unwrap_or_else(|_| ObjectPath::from_static_str_unchecked("/dev/sonance/track/0"))
}

fn ov(v: Value<'_>) -> Option<OwnedValue> {
    OwnedValue::try_from(v).ok()
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    fn next(&self) {
        self.send(Cmd::Next);
    }
    fn previous(&self) {
        self.send(Cmd::Previous);
    }
    fn pause(&self) {
        self.send(Cmd::Pause);
    }
    fn play_pause(&self) {
        self.send(Cmd::PlayPause);
    }
    fn stop(&self) {
        self.send(Cmd::Stop);
    }
    fn play(&self) {
        self.send(Cmd::Play);
    }
    fn seek(&self, offset: i64) {
        self.send(Cmd::Seek(offset));
    }
    fn set_position(&self, _track_id: ObjectPath<'_>, position: i64) {
        self.send(Cmd::SetPosition(position));
    }
    fn open_uri(&self, _uri: String) {}

    #[zbus(signal)]
    async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> String {
        let s = self.get();
        if s.playing {
            "Playing"
        } else if s.paused {
            "Paused"
        } else {
            "Stopped"
        }
        .into()
    }
    #[zbus(property)]
    fn loop_status(&self) -> String {
        let l = self.get().loop_status;
        if l.is_empty() { "None".into() } else { l }
    }
    #[zbus(property)]
    fn set_loop_status(&self, v: String) {
        self.send(Cmd::Loop(v));
    }
    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }
    #[zbus(property)]
    fn set_rate(&self, _v: f64) {}
    #[zbus(property)]
    fn shuffle(&self) -> bool {
        self.get().shuffle
    }
    #[zbus(property)]
    fn set_shuffle(&self, v: bool) {
        self.send(Cmd::Shuffle(v));
    }
    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        let s = self.get();
        let mut m = HashMap::new();
        let mut put = |k: &str, v: Value<'_>| {
            if let Some(v) = ov(v) {
                m.insert(k.to_string(), v);
            }
        };
        put("mpris:trackid", Value::from(track_path(s.track_no)));
        if s.length_us > 0 {
            put("mpris:length", Value::from(s.length_us));
        }
        if !s.title.is_empty() {
            put("xesam:title", Value::from(s.title.clone()));
        }
        if !s.artist.is_empty() {
            put("xesam:artist", Value::from(vec![s.artist.clone()]));
        }
        if !s.album.is_empty() {
            put("xesam:album", Value::from(s.album.clone()));
        }
        if let Some(art) = s.art {
            put("mpris:artUrl", Value::from(art));
        }
        m
    }
    #[zbus(property)]
    fn volume(&self) -> f64 {
        self.get().volume
    }
    #[zbus(property)]
    fn set_volume(&self, v: f64) {
        self.send(Cmd::Volume(v.clamp(0.0, 1.0)));
    }
    /// Polled by shells, so no change signal (as the spec asks).
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        self.get().position_us
    }
    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }
    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }
    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        self.get().can_skip
    }
    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        self.get().can_skip
    }
    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_seek(&self) -> bool {
        self.get().length_us > 0
    }
    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }
}

#[derive(Clone)]
pub struct Mpris {
    conn: zbus::Connection,
    state: Shared,
}

impl Mpris {
    /// Claims `org.mpris.MediaPlayer2.sonance` and serves the two interfaces.
    pub async fn start(tx: UnboundedSender<Cmd>) -> Result<Self> {
        let state: Shared = Arc::default();
        let conn = zbus::connection::Builder::session()?
            .name("org.mpris.MediaPlayer2.sonance")?
            .serve_at(PATH, Root { tx: tx.clone() })?
            .serve_at(PATH, Player { tx, state: state.clone() })?
            .build()
            .await?;
        Ok(Self { conn, state })
    }

    /// Stores a new snapshot and tells listeners what changed.
    pub async fn update(&self, new: State) -> Result<()> {
        let old = std::mem::replace(&mut *self.state.lock().unwrap(), new.clone());
        if old == new {
            return Ok(());
        }
        let iface = self.conn.object_server().interface::<_, Player>(PATH).await?;
        let p = iface.get().await;
        let em = iface.signal_emitter();
        if (old.playing, old.paused) != (new.playing, new.paused) {
            p.playback_status_changed(em).await?;
        }
        if (&old.title, &old.artist, &old.album, &old.art, old.length_us, old.track_no) != (&new.title, &new.artist, &new.album, &new.art, new.length_us, new.track_no) {
            p.metadata_changed(em).await?;
            p.can_seek_changed(em).await?;
        }
        if old.volume != new.volume {
            p.volume_changed(em).await?;
        }
        if old.shuffle != new.shuffle {
            p.shuffle_changed(em).await?;
        }
        if old.loop_status != new.loop_status {
            p.loop_status_changed(em).await?;
        }
        if old.can_skip != new.can_skip {
            p.can_go_next_changed(em).await?;
            p.can_go_previous_changed(em).await?;
        }
        // A jump in position (a seek, a new track) is announced; steady playback isn't.
        let expected = old.position_us + 1_000_000;
        if (new.position_us - expected).abs() > 2_500_000 && old.track_no == new.track_no {
            Player::seeked(em, new.position_us).await?;
        }
        Ok(())
    }
}

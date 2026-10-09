mod audio;
mod config;
mod room;
mod sonos;
mod spotify;
mod ui;

use std::sync::{Arc, Mutex, OnceLock};

use config::Config;
use sonos::Sonos;
use spotify::Spotify;

pub fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().expect("tokio runtime"))
}

/// Everything the UI talks to; cheap to clone and safe to move into tasks.
#[derive(Clone)]
pub struct Core {
    pub sonos: Sonos,
    pub audio: audio::Engine,
    pub cfg: Arc<Mutex<Config>>,
    pub spotify: Spotify,
}

impl Core {
    fn new() -> Self {
        let sonos = Sonos::new();
        let cfg = Arc::new(Mutex::new(Config::load()));
        let spotify = Spotify::new(sonos.soap.http().clone(), cfg.clone());
        Self { sonos, cfg, spotify, audio: audio::Engine::new() }
    }
}

/// `sonance --probe`: read-only dump of what the app sees, for diagnosing a network.
async fn probe(core: Core) -> anyhow::Result<()> {
    let known = core.cfg.lock().unwrap().known_ips.clone();
    let ip = sonos::discovery::find_speaker(&core.sonos.soap, &known).await?;
    println!("found speaker at {ip}");
    let s = &core.sonos;
    for g in s.groups(&ip).await? {
        println!("group {:?} coord={} members={:?}", g.name(), g.coordinator.ip, g.members.iter().map(|m| &m.name).collect::<Vec<_>>());
        println!("  status {:?}", s.status(&g.coordinator.ip).await?);
        println!("  queue {} items", s.queue(&g.coordinator.ip).await?.len());
        println!("  sleep {:?}", s.sleep_timer(&g.coordinator.ip).await?);
        for m in &g.members {
            println!("  {} vol={:?} eq={:?}", m.name, s.volume(&m.ip).await, s.eq(&m.ip).await);
        }
    }
    let favs = s.favorites(&ip).await?;
    for f in &favs {
        println!("favorite {:?} -> {}", f.title, if f.uri.is_empty() { "(Sonos-app-only shortcut)" } else { &f.uri });
    }
    let g = &s.groups(&ip).await?[0];
    let q = s.queue(&g.coordinator.ip).await?;
    let acc = Sonos::spotify_account(&favs, &s.alarms(&ip).await?, &q).unwrap_or_default();
    println!("spotify account {acc:?}");
    println!("units {:?}", s.units(&ip).await?);
    println!("playlists {:?}", s.playlists(&ip).await.map(|p| p.len()));
    println!("alarms {:?}", s.alarms(&ip).await?);
    Ok(())
}

fn main() {
    let core = Core::new();
    if std::env::args().any(|a| a == "--probe") {
        if let Err(e) = rt().block_on(probe(core)) {
            eprintln!("probe failed: {e:#}");
        }
        return;
    }
    ui::run(core);
}

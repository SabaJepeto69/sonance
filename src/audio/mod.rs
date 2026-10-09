//! Audio engine: the "Sonance" PipeWire output, Wi-Fi streaming to Sonos,
//! Bluetooth speakers with per-device delay, and microphone measurement.
// The UI wires these up next; until then most of the API is unused.
#![allow(dead_code, unused_imports)]

mod bluetooth;
mod dsp;
#[cfg(test)]
mod live_tests;
mod measure;
mod pw;
mod routing;
mod sink;
mod wifi;

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, OnceCell};

pub use bluetooth::BtDevice;
pub use measure::Measurement;
pub use routing::Route;
pub use wifi::{StreamUrls, lan_ip};

/// Node name of the virtual output everything is played into.
pub const SINK: &str = "sonance";

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    /// Stale helpers from a crashed run are reaped once, before we spawn our own.
    reaped: OnceCell<()>,
    bus: OnceCell<zbus::Connection>,
    agent: OnceCell<()>,
    /// Two sweeps at once would hear each other.
    measuring: Mutex<()>,
}

#[derive(Default)]
struct State {
    /// pipewire-pulse module index, only if we loaded the sink ourselves.
    sink_module: Option<String>,
    prev_default: Option<String>,
    routes: HashMap<String, routing::Active>,
    wifi: Option<wifi::Server>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                reaped: OnceCell::new(),
                bus: OnceCell::new(),
                agent: OnceCell::new(),
                measuring: Mutex::new(()),
            }),
        }
    }

    async fn reap_stale(&self) {
        self.inner.reaped.get_or_init(|| async { pw::kill_stale().await }).await;
    }

    /// Tears down everything this Engine started: loopbacks, stream, the sink (if we created it),
    /// and gives the previous default output back.
    pub async fn shutdown(&self) {
        self.clear_routes().await;
        self.stop_wifi_stream().await;
        let _ = self.set_default_output(false).await;
        let module = self.inner.state.lock().await.sink_module.take();
        if let Some(m) = module {
            let _ = pw::run("pactl", &["unload-module", &m]).await;
        }
    }
}

//! The 3D room: speaker placement, listening spots, and the maths that turns
//! them into per-speaker settings.
//!
//! Coordinates are metres. x runs along the room's width, z along its depth
//! (both on the floor), y is up. A speaker's yaw is the compass direction it
//! faces on the floor plane: 0° looks toward -z, 90° toward +x.
//!
//! What Sonos lets us change is limited: per-room volume, the left/right
//! balance of a stereo pair, and bass. It plays every speaker in a group at
//! the same instant, so arrival-time differences can only be corrected on
//! Bluetooth speakers, whose delay lines Sonance controls.

pub mod plan;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RoomBox {
    pub width: f32,
    pub depth: f32,
    pub height: f32,
}

impl Default for RoomBox {
    fn default() -> Self {
        Self { width: 4.5, depth: 3.8, height: 2.6 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Kind {
    /// One physical Sonos box. `room` is the visible member it plays in;
    /// `channel` is LF/RF for half of a stereo pair.
    Sonos { uuid: String, room: String, channel: Option<String> },
    Bluetooth { mac: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Speaker {
    /// "sonos:<uuid>" or "bt:<mac>".
    pub id: String,
    pub name: String,
    pub kind: Kind,
    pub pos: [f32; 3],
    pub yaw: f32,
    /// Speaker model, e.g. "Sonos Era 100"; sizes it in the 3D view.
    #[serde(default)]
    pub model: String,
}

impl Speaker {
    /// Rough (width, height, depth) in metres, for drawing.
    pub fn size(&self) -> (f32, f32, f32) {
        let m = self.model.to_lowercase();
        let has = |k: &str| m.contains(k);
        if has("arc") {
            (1.14, 0.09, 0.12)
        } else if has("beam") || has("ray") || has("playbar") || has("playbase") {
            (0.65, 0.07, 0.10)
        } else if has("sub") {
            (0.40, 0.39, 0.16)
        } else if has("move") {
            (0.16, 0.24, 0.13)
        } else if has("roam") {
            (0.06, 0.17, 0.06)
        } else if has("era 300") {
            (0.26, 0.16, 0.19)
        } else if has("five") || has("play:5") {
            (0.36, 0.20, 0.15)
        } else if has("one") || has("play:1") {
            (0.12, 0.16, 0.12)
        } else {
            (0.12, 0.18, 0.13)
        }
    }

    pub fn is_bluetooth(&self) -> bool {
        matches!(self.kind, Kind::Bluetooth { .. })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Spot {
    pub id: String,
    pub name: String,
    /// Ear position; y defaults to seated ear height.
    pub pos: [f32; 3],
    /// Mic results for this spot, by speaker id.
    #[serde(default)]
    pub measured: HashMap<String, Measured>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Measured {
    /// Time from handing audio to the output until it reached the mic.
    pub latency_ms: f32,
    /// Level at the spot (dBFS at the mic) with the speaker at `at_volume`.
    pub level_db: f32,
    pub at_volume: u8,
}

/// What tuning changed, so "regular" can undo exactly that. Volume is kept
/// relative (steps added) so the user's own volume changes survive.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Baseline {
    /// Room uuid → bass before tuning.
    pub bass: HashMap<String, i32>,
    /// Room uuid → (LF, RF) channel volumes of a stereo pair before tuning.
    pub balance: HashMap<String, (u8, u8)>,
    /// Room uuid → volume steps tuning has currently added.
    pub applied: HashMap<String, i32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Layout {
    pub room: RoomBox,
    pub speakers: Vec<Speaker>,
    pub spots: Vec<Spot>,
    pub active_spot: Option<String>,
    /// The toggle: tune for the active spot, or play "regular".
    pub tuned: bool,
    pub baseline: Option<Baseline>,
}

fn path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("sonance").join("room.json")
}

impl Layout {
    pub fn load() -> Self {
        std::fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        let p = path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(tmp, p);
            }
        }
    }

    pub fn active(&self) -> Option<&Spot> {
        let id = self.active_spot.as_ref()?;
        self.spots.iter().find(|s| &s.id == id)
    }

    pub fn speaker(&self, id: &str) -> Option<&Speaker> {
        self.speakers.iter().find(|s| s.id == id)
    }

    /// Somewhere sensible for a newly added object: spread along the back wall.
    pub fn free_spot_for_speaker(&self) -> [f32; 3] {
        let n = self.speakers.len() as f32;
        let x = (0.6 + n * 1.1).min(self.room.width - 0.4);
        [x, 0.75, 0.35]
    }

    pub fn add_spot(&mut self, name: &str) -> String {
        let id = format!("spot-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0));
        let pos = [self.room.width / 2.0, 1.1, self.room.depth * 0.65];
        self.spots.push(Spot { id: id.clone(), name: name.to_string(), pos, measured: HashMap::new() });
        if self.active_spot.is_none() {
            self.active_spot = Some(id.clone());
        }
        id
    }

    /// Keeps everything inside the walls after a resize or a drag.
    pub fn clamp(&mut self) {
        let r = self.room.clone();
        let fit = |p: &mut [f32; 3]| {
            p[0] = p[0].clamp(0.1, r.width - 0.1);
            p[1] = p[1].clamp(0.0, r.height - 0.1);
            p[2] = p[2].clamp(0.1, r.depth - 0.1);
        };
        for s in &mut self.speakers {
            fit(&mut s.pos);
        }
        for s in &mut self.spots {
            fit(&mut s.pos);
        }
    }
}

/// Unit vector a speaker with this yaw faces, on the floor plane.
pub fn facing(yaw_deg: f32) -> [f32; 2] {
    let a = yaw_deg.to_radians();
    [a.sin(), -a.cos()]
}

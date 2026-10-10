//! From positions (and, when available, mic measurements) to settings.
//!
//! Loudness at the spot follows the inverse-square law (-6 dB per doubling of
//! distance) plus an off-axis loss for speakers not facing the listener.
//! Every speaker is turned *down* to the quietest one's level, never up, so
//! tuning can't push anything into distortion. Measurements from the mic
//! replace the model wherever they exist.

use std::collections::HashMap;

use super::{facing, Kind, Layout, Speaker, Spot};

/// Sonos volume steps are roughly this many dB apart in the useful range.
pub const DB_PER_STEP: f32 = 0.6;
/// A wall closer than this reinforces a speaker's bass by about 3 dB.
const WALL_NEAR: f32 = 0.6;
/// Rough Bluetooth playout latency before the mic has measured the real one.
const BT_LATENCY_GUESS_MS: f32 = 180.0;
const SPEED_OF_SOUND: f32 = 343.0;

#[derive(Clone, Debug, PartialEq)]
pub struct SpeakerPlan {
    pub id: String,
    pub name: String,
    pub distance: f32,
    /// Degrees between where it faces and where the listener is.
    pub off_axis: f32,
    /// Degrees to rotate it to face the listener; positive = clockwise from above.
    pub turn: f32,
    pub walls: u8,
    /// Level at the spot before correction, in dB relative to 1 m on-axis.
    pub level_db: f32,
    pub measured: bool,
    /// When its sound arrives at the spot, flight time plus device latency.
    pub arrival_ms: f32,
    /// Measured octave-band response at the spot, if calibrated.
    pub bands: Option<Vec<f32>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    pub speakers: Vec<SpeakerPlan>,
    /// Sonos room uuid → volume steps to add to the baseline (≤ 0).
    pub volume_steps: HashMap<String, i32>,
    /// Sonos room uuid → (LF, RF) channel volumes for a stereo pair.
    pub balance: HashMap<String, (u8, u8)>,
    /// Sonos room uuid → bass steps to add to the baseline: from the measured
    /// response when calibrated, else from nearby walls.
    pub bass_steps: HashMap<String, i32>,
    /// Sonos room uuid → treble steps to add to the baseline (measured only).
    pub treble_steps: HashMap<String, i32>,
    /// Route key → room-EQ band gains (dB), from measurements.
    pub eq: HashMap<String, Vec<f32>>,
    /// Largest EQ boost anywhere; Bluetooth routes are all lowered by this
    /// much so boosts can't clip, and their relative levels stay intact.
    pub headroom_db: f32,
    /// Route key (Sonos room uuid or Bluetooth MAC) → (delay ms, gain dB) when
    /// everything plays over Bluetooth.
    pub bt_routes: HashMap<String, (f32, f32)>,
    /// Arrival spread across all speakers, which Sonos-over-Wi-Fi can't fix.
    pub spread_ms: f32,
}

/// Which delay line / volume control a speaker belongs to.
pub fn route_key(s: &Speaker) -> String {
    match &s.kind {
        Kind::Sonos { room, .. } => room.clone(),
        Kind::Bluetooth { mac } => mac.clone(),
    }
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn len(v: [f32; 3]) -> f32 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

/// Off-axis loss of a small full-range speaker, averaged over the band:
/// nothing on-axis, ~4 dB at 90°, ~8 dB from behind.
fn directivity_db(off_axis_deg: f32) -> f32 {
    4.0 * (1.0 - off_axis_deg.to_radians().cos())
}

fn wrap180(a: f32) -> f32 {
    (a + 540.0).rem_euclid(360.0) - 180.0
}

/// Power-sums levels in dB (two speakers 0 dB each make +3 dB).
fn power_sum(levels: &[f32]) -> f32 {
    10.0 * levels.iter().map(|l| 10f32.powf(l / 10.0)).sum::<f32>().max(1e-9).log10()
}

/// When sound from `s` would reach `spot` going by the model alone: flight
/// time, plus a typical Bluetooth delay.
pub fn model_arrival_ms(s: &Speaker, spot: &Spot) -> f32 {
    let distance = len(sub(spot.pos, s.pos)).max(0.2);
    distance / SPEED_OF_SOUND * 1000.0 + if s.is_bluetooth() { BT_LATENCY_GUESS_MS } else { 0.0 }
}

pub fn speaker_plan(layout: &Layout, s: &Speaker, spot: &Spot) -> SpeakerPlan {
    let to = sub(spot.pos, s.pos);
    let distance = len(to).max(0.2);
    let f = facing(s.yaw);
    let cos = ((f[0] * to[0] + f[1] * to[2]) / distance).clamp(-1.0, 1.0);
    let off_axis = cos.acos().to_degrees();
    let target_yaw = to[0].atan2(-to[2]).to_degrees();
    let turn = wrap180(target_yaw - s.yaw);

    let r = &layout.room;
    let walls = [s.pos[0], r.width - s.pos[0], s.pos[2], r.depth - s.pos[2]].iter().filter(|d| **d < WALL_NEAR).count() as u8;

    let flight = distance / SPEED_OF_SOUND * 1000.0;
    let bands = spot.measured.get(&s.id).map(|m| m.bands_db.clone()).filter(|b| b.len() == crate::audio::BANDS_HZ.len());
    let (level_db, arrival_ms, measured) = match spot.measured.get(&s.id) {
        Some(m) => (m.level_db, m.latency_ms, true),
        None => (
            -20.0 * distance.log10() - directivity_db(off_axis),
            flight + if s.is_bluetooth() { BT_LATENCY_GUESS_MS } else { 0.0 },
            false,
        ),
    };
    SpeakerPlan { id: s.id.clone(), name: s.name.clone(), distance, off_axis, turn, walls, level_db, measured, arrival_ms, bands }
}

/// `include` picks the speakers currently playing (the selected group plus
/// any Bluetooth speakers switched on).
pub fn compute(layout: &Layout, spot: &Spot, include: impl Fn(&Speaker) -> bool) -> Plan {
    let speakers: Vec<&Speaker> = layout.speakers.iter().filter(|s| include(s)).collect();
    let mut plans: Vec<SpeakerPlan> = speakers.iter().map(|s| speaker_plan(layout, s, spot)).collect();
    remove_mic_colour(&mut plans);
    let mut plan = Plan::default();
    if plans.is_empty() {
        return plan;
    }

    // Group into what can be controlled: a Sonos room (both halves of a pair) or a BT speaker.
    let mut groups: HashMap<String, Vec<(&Speaker, &SpeakerPlan)>> = HashMap::new();
    for (s, p) in speakers.iter().zip(&plans) {
        groups.entry(route_key(s)).or_default().push((s, p));
    }
    let group_level: HashMap<&String, f32> = groups.iter().map(|(k, v)| (k, power_sum(&v.iter().map(|(_, p)| p.level_db).collect::<Vec<_>>()))).collect();
    let quietest = group_level.values().cloned().fold(f32::INFINITY, f32::min);

    for (key, members) in &groups {
        let cut = quietest - group_level[key];
        // Room EQ from what the mic heard: both halves of a pair power-averaged.
        let measured: Vec<&Vec<f32>> = members.iter().filter_map(|(_, p)| p.bands.as_ref()).collect();
        let correction = (!measured.is_empty()).then(|| crate::audio::correction(&average_bands(&measured), layout.eq_strength()));
        if let Some(c) = &correction {
            plan.eq.insert(key.clone(), c.clone());
        }
        match &members[0].0.kind {
            Kind::Sonos { .. } => {
                plan.volume_steps.insert(key.clone(), (cut / DB_PER_STEP).round() as i32);
                match &correction {
                    // Sonos only has bass and treble; the measured curve is mapped onto those.
                    Some(c) => {
                        let (bass, treble) = crate::audio::sonos_tone(c);
                        plan.bass_steps.insert(key.clone(), bass);
                        plan.treble_steps.insert(key.clone(), treble);
                    }
                    None => {
                        // Each nearby wall adds ~3 dB of boundary gain; Sonos bass steps are ~1.5 dB.
                        let walls = members.iter().map(|(_, p)| p.walls as f32).sum::<f32>() / members.len() as f32;
                        plan.bass_steps.insert(key.clone(), -(walls * 3.0 / 1.5).round().clamp(0.0, 6.0) as i32);
                    }
                }
                let side = |ch: &str| members.iter().find(|(s, _)| matches!(&s.kind, Kind::Sonos { channel: Some(c), .. } if c == ch)).map(|(_, p)| p.level_db);
                if let (Some(lf), Some(rf)) = (side("LF"), side("RF")) {
                    // Turn the louder half down; channel volume is linear in amplitude.
                    let diff = lf - rf;
                    let down = |db: f32| (100.0 * 10f32.powf(-db.abs() / 20.0)).round() as u8;
                    plan.balance.insert(key.clone(), if diff > 0.0 { (down(diff), 100) } else { (100, down(diff)) });
                }
            }
            Kind::Bluetooth { .. } => {
                plan.bt_routes.insert(key.clone(), (0.0, cut));
            }
        }
    }

    // Line everything up with the latest arrival. Over Bluetooth every route,
    // Sonos rooms included, gets its own delay line.
    let arrival = |members: &Vec<(&Speaker, &SpeakerPlan)>| members.iter().map(|(_, p)| p.arrival_ms).sum::<f32>() / members.len() as f32;
    let latest = groups.values().map(arrival).fold(f32::MIN, f32::max);
    let earliest = groups.values().map(arrival).fold(f32::MAX, f32::min);
    plan.spread_ms = latest - earliest;
    for (key, members) in &groups {
        let delay = latest - arrival(members);
        let gain = plan.bt_routes.get(key).map(|r| r.1).unwrap_or(0.0);
        plan.bt_routes.insert(key.clone(), (delay, gain));
    }
    plan.headroom_db = plan.eq.values().flatten().cloned().fold(0.0, f32::max);
    plan.speakers = plans;
    plan
}

/// Bands at or above this index (4 kHz) are treated as mic-coloured when every
/// speaker shows the same deviation there.
const MIC_BANDS_FROM: usize = 6;

/// Webcam and laptop mics are far from flat in the treble: on a test system
/// three different speakers all measured +6 to +9 dB at 4–8 kHz, which is the
/// mic, not the speakers. What every speaker at a spot has in common up there
/// is taken out. Bass is left alone, since room modes really do differ from
/// one speaker position to the next.
fn remove_mic_colour(plans: &mut [SpeakerPlan]) {
    let measured: Vec<usize> = plans.iter().enumerate().filter(|(_, p)| p.bands.is_some()).map(|(i, _)| i).collect();
    if measured.len() < 2 {
        return;
    }
    for band in MIC_BANDS_FROM..crate::audio::BANDS_HZ.len() {
        let mut vals: Vec<f32> = measured.iter().filter_map(|&i| plans[i].bands.as_ref()?.get(band).copied()).filter(|v| v.is_finite()).collect();
        if vals.len() < 2 {
            continue;
        }
        vals.sort_by(f32::total_cmp);
        let common = vals[vals.len() / 2];
        for &i in &measured {
            if let Some(v) = plans[i].bands.as_mut().and_then(|b| b.get_mut(band)) {
                *v -= common;
            }
        }
    }
}

/// Power-averages octave responses, skipping bands a measurement couldn't hear.
fn average_bands(bands: &[&Vec<f32>]) -> Vec<f32> {
    (0..crate::audio::BANDS_HZ.len())
        .map(|i| {
            let vals: Vec<f32> = bands.iter().filter_map(|b| b.get(i).copied()).filter(|v| v.is_finite()).collect();
            if vals.is_empty() { f32::NAN } else { 10.0 * (vals.iter().map(|v| 10f32.powf(v / 10.0)).sum::<f32>() / vals.len() as f32).log10() }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room::{Layout, Spot};

    fn sonos(id: &str, room: &str, ch: Option<&str>, pos: [f32; 3], yaw: f32) -> Speaker {
        Speaker { id: id.into(), name: id.into(), kind: Kind::Sonos { uuid: id.into(), room: room.into(), channel: ch.map(String::from) }, pos, yaw, model: String::new() }
    }

    fn spot(pos: [f32; 3]) -> Spot {
        Spot { id: "s".into(), name: "s".into(), pos, measured: Default::default() }
    }

    #[test]
    fn symmetric_pair_is_balanced() {
        let mut l = Layout::default();
        l.speakers = vec![sonos("L", "pair", Some("LF"), [1.5, 1.0, 0.5], 180.0), sonos("R", "pair", Some("RF"), [3.0, 1.0, 0.5], 180.0)];
        let p = compute(&l, &spot([2.25, 1.0, 3.0]), |_| true);
        assert_eq!(p.balance["pair"], (100, 100));
        assert_eq!(p.volume_steps["pair"], 0);
    }

    #[test]
    fn nearer_room_is_turned_down() {
        let mut l = Layout::default();
        l.speakers = vec![sonos("near", "a", None, [2.0, 1.0, 2.0], 180.0), sonos("far", "b", None, [2.0, 1.0, 0.2], 180.0)];
        let p = compute(&l, &spot([2.0, 1.0, 3.0]), |_| true);
        assert!(p.volume_steps["a"] < 0);
        assert_eq!(p.volume_steps["b"], 0);
        // The far one's sound arrives later, so the near one waits for it.
        assert!(p.bt_routes["a"].0 > 0.0 && p.bt_routes["b"].0 == 0.0);
    }

    #[test]
    fn turn_points_at_listener() {
        let l = Layout::default();
        // Facing -z (yaw 0), listener straight to its right (+x): turn 90° clockwise.
        let s = sonos("x", "a", None, [1.0, 1.0, 1.0], 0.0);
        let sp = speaker_plan(&l, &s, &spot([2.0, 1.0, 1.0]));
        assert!((sp.turn - 90.0).abs() < 0.5, "{}", sp.turn);
        assert!((sp.off_axis - 90.0).abs() < 0.5);
    }

    #[test]
    fn mic_colour_is_shared_treble() {
        // Real measurements (webcam mic): all three speakers read +6..9 dB at 4–8 kHz.
        let real = [
            [15.2, 7.4, 7.1, 0.4, -0.6, 0.2, 6.0, 6.0],
            [3.0, -5.4, -1.3, -3.4, 1.6, 1.7, 9.2, 8.6],
            [-0.1, 1.0, -1.3, -0.2, 0.5, -0.3, 6.1, 5.7],
        ];
        let l = Layout::default();
        let mut plans: Vec<SpeakerPlan> = real
            .iter()
            .map(|b| {
                let mut p = speaker_plan(&l, &sonos("x", "a", None, [1.0, 1.0, 1.0], 0.0), &spot([2.0, 1.0, 2.0]));
                p.bands = Some(b.to_vec());
                p
            })
            .collect();
        remove_mic_colour(&mut plans);
        let b = |i: usize| plans[i].bands.clone().unwrap();
        // The shared treble rise is gone; only the left Era stays brighter than the rest.
        assert!(b(0)[6].abs() < 0.2 && b(2)[6].abs() < 0.2 && (b(1)[6] - 3.1).abs() < 0.01);
        // Bass, where positions genuinely differ, is untouched.
        assert_eq!(b(0)[0], 15.2);
    }

    #[test]
    fn corner_cuts_bass() {
        let mut l = Layout::default();
        l.speakers = vec![sonos("c", "a", None, [0.2, 1.0, 0.2], 135.0)];
        let p = compute(&l, &spot([2.0, 1.0, 2.0]), |_| true);
        assert_eq!(p.bass_steps["a"], -4);
    }
}

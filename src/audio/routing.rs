//! Delay lines from the `sonance` monitor to each Bluetooth sink, run by
//! Sonance itself rather than pw-loopback.
//!
//! Each route is two plain streams, a recorder on the sonance monitor and a
//! player on the speaker, with a ring buffer between them; the audio held in
//! the ring *is* the delay. That fixes what pw-loopback couldn't:
//! - its `--delay` is a target for total latency, so it swallowed any delay
//!   shorter than the speaker's own (~250 ms over Bluetooth);
//! - changing a delay meant restarting it, which briefly stopped the Bluetooth
//!   stream and made the speaker re-buffer to a different latency;
//! - the sonance clock and the Bluetooth clock drift apart (~1 ms/s measured),
//!   and nothing pulled the delay back.
//! Here a control loop holds the ring at its target: one frame is dropped or
//! repeated per chunk to follow clock drift, big changes jump with a short
//! fade, and the player is fed silence rather than starved, so the Bluetooth
//! link never stops.

use std::collections::{HashSet, VecDeque};
use std::os::fd::AsRawFd;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Child;
use tokio::task::JoinHandle;

use super::{Engine, SINK, pw};

const RATE: usize = 48_000;
/// Frames written to the player per step.
const CHUNK: usize = 256;
/// Within this the drift loop nudges a frame at a time; beyond it, it jumps.
const NUDGE_LIMIT: i64 = (RATE / 50) as i64; // 20 ms
/// Dead band before nudging, so the loop doesn't chatter.
const DEAD_BAND: i64 = (RATE / 1000) as i64; // 1 ms
const FADE: usize = 128;
/// Every route holds this much on top of its own delay: the recorder delivers
/// in bursts, so a ring held near empty would keep running dry. It's the same
/// on every route, so the differences between speakers stay exact.
const FLOOR_MS: f32 = 40.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    /// PipeWire node name of the target sink.
    pub sink: String,
    pub delay_ms: f32,
    pub gain_db: f32,
}

/// Shared between the control side (set_routes) and the pump tasks.
struct Ctl {
    target_frames: AtomicI64,
    gain: AtomicU32,
    /// Last observed ring fill, in frames, for diagnostics and tests.
    fill: AtomicI64,
}

pub(super) struct Active {
    delay_ms: f32,
    gain_db: f32,
    ctl: Arc<Ctl>,
    rec: Child,
    play: Child,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Active {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

#[cfg(test)]
impl Active {
    pub(super) fn child_id(&self) -> Option<u32> {
        self.play.id()
    }
    pub(super) fn fill_ms(&self) -> f32 {
        self.ctl.fill.load(Ordering::Relaxed) as f32 * 1000.0 / RATE as f32
    }
}

/// `sonance-route-<mac>` for Bluetooth sinks, a sanitised sink name otherwise.
fn route_name(sink: &str) -> String {
    let tag = sink.strip_prefix("bluez_output.").and_then(|r| r.split('.').next()).unwrap_or(sink);
    let tag: String = tag.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    format!("sonance-route-{tag}")
}

fn frames(ms: f32) -> i64 {
    (ms.max(0.0) / 1000.0 * RATE as f32).round() as i64
}

/// Ring fill the control loop aims for, for a route's delay.
fn target_for(delay_ms: f32) -> i64 {
    frames(delay_ms + FLOOR_MS)
}

fn gain_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Keeps the player's stdin pipe tiny: anything queued in it is latency the
/// control loop can't see.
fn shrink_pipe(fd: i32) {
    // SAFETY: fcntl on a pipe fd we own; failure just leaves the default size.
    unsafe {
        libc::fcntl(fd, libc::F_SETPIPE_SZ, 4096);
    }
}

async fn start(route: &Route) -> Result<Active> {
    let name = route_name(&route.sink);
    // dont-fallback/dont-reconnect: if either end vanishes the stream must go quiet,
    // never re-attach to the default device (the room speakers, or the microphone).
    let common = "node.dont-fallback=true node.dont-reconnect=true state.restore-props=false state.restore-target=false";
    let fmt = ["--raw", "--rate", "48000", "--channels", "2", "--format", "f32", "--latency", "256"];
    let mut rec = pw::command("pw-cat")
        .arg("--record")
        .args(fmt)
        .args(["--target", SINK, "-P"])
        .arg(format!("{{ node.name={name}-in stream.capture.sink=true {common} }}"))
        .arg("-")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("couldn't start the recorder")?;
    let mut play = pw::command("pw-cat")
        .arg("--playback")
        .args(fmt)
        .args(["--target", &route.sink, "-P"])
        .arg(format!("{{ node.name={name} node.description=\"Sonance → {}\" {common} }}", route.sink))
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("couldn't start the player")?;
    let rx = rec.stdout.take().context("recorder stdout")?;
    let tx = play.stdin.take().context("player stdin")?;
    shrink_pipe(tx.as_raw_fd());

    let ctl = Arc::new(Ctl {
        target_frames: AtomicI64::new(target_for(route.delay_ms)),
        gain: AtomicU32::new(gain_lin(route.gain_db).to_bits()),
        fill: AtomicI64::new(0),
    });
    let ring: Arc<Mutex<VecDeque<[f32; 2]>>> = Arc::new(Mutex::new(VecDeque::with_capacity(RATE * 4)));
    let tasks = vec![tokio::spawn(read_side(rx, ring.clone())), tokio::spawn(write_side(tx, ring, ctl.clone()))];

    // Wait for the player node, so callers can rely on the route existing.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if let Some(status) = play.try_wait()? {
            bail!("delay line to {} exited ({status})", route.sink);
        }
        if pw::find_node(&name).await?.is_some() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("delay line to {} didn't come up", route.sink);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Ok(Active { delay_ms: route.delay_ms, gain_db: route.gain_db, ctl, rec, play, tasks })
}

async fn read_side(mut rx: tokio::process::ChildStdout, ring: Arc<Mutex<VecDeque<[f32; 2]>>>) {
    let mut buf = vec![0u8; 8 * 1024];
    let mut carry: Vec<u8> = Vec::new();
    loop {
        let n = match rx.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        carry.extend_from_slice(&buf[..n]);
        let whole = carry.len() / 8 * 8;
        {
            let mut r = ring.lock().unwrap();
            for f in carry[..whole].chunks_exact(8) {
                let l = f32::from_le_bytes([f[0], f[1], f[2], f[3]]);
                let rr = f32::from_le_bytes([f[4], f[5], f[6], f[7]]);
                r.push_back([l, rr]);
            }
            // Never hold more than a few seconds, whatever happens downstream.
            let excess = r.len().saturating_sub(RATE * 4);
            r.drain(..excess);
        }
        carry.drain(..whole);
    }
}

/// What the writer should do with the ring this step.
#[derive(Debug, PartialEq)]
enum Step {
    /// Not enough buffered for the delay yet: play silence and let it fill.
    Wait,
    /// Far too much buffered: throw this many frames away.
    Jump(usize),
    /// Slightly behind or ahead: drop (+1) or repeat (-1) one frame.
    Nudge(i8),
    Steady,
}

fn decide(fill: i64, target: i64) -> Step {
    let err = fill - target;
    if fill < target - NUDGE_LIMIT {
        Step::Wait
    } else if err > NUDGE_LIMIT {
        Step::Jump(err as usize)
    } else if err > DEAD_BAND {
        Step::Nudge(1)
    } else if err < -DEAD_BAND {
        Step::Nudge(-1)
    } else {
        Step::Steady
    }
}

async fn write_side(mut tx: tokio::process::ChildStdin, ring: Arc<Mutex<VecDeque<[f32; 2]>>>, ctl: Arc<Ctl>) {
    let mut out: Vec<[f32; 2]> = Vec::with_capacity(CHUNK + 1);
    let mut bytes = Vec::with_capacity((CHUNK + 1) * 8);
    let mut gain = f32::from_bits(ctl.gain.load(Ordering::Relaxed));
    let mut fade_in = 0usize;
    loop {
        out.clear();
        {
            let mut r = ring.lock().unwrap();
            let target = ctl.target_frames.load(Ordering::Relaxed);
            ctl.fill.store(r.len() as i64, Ordering::Relaxed);
            match decide(r.len() as i64, target) {
                Step::Wait => {
                    if fade_in == 0 {
                        fade_in = FADE;
                    }
                }
                Step::Jump(n) => {
                    let n = n.min(r.len());
                    r.drain(..n);
                    fade_in = FADE;
                }
                Step::Nudge(1) => {
                    r.pop_front();
                }
                Step::Nudge(_) => {
                    if let Some(f) = r.front().copied() {
                        out.push(f);
                    }
                }
                Step::Steady => {}
            }
            if decide(r.len() as i64, target) != Step::Wait {
                let take = (CHUNK - out.len()).min(r.len());
                out.extend(r.drain(..take));
            }
        }
        // Silence keeps the Bluetooth link streaming while the ring fills.
        out.resize(CHUNK, [0.0, 0.0]);

        let want = f32::from_bits(ctl.gain.load(Ordering::Relaxed));
        bytes.clear();
        for (i, f) in out.iter().enumerate() {
            // Glide gain changes over one chunk; fade in after any jump.
            gain += (want - gain) / (CHUNK - i) as f32;
            let fade = if fade_in > 0 { 1.0 - fade_in.saturating_sub(i) as f32 / FADE as f32 } else { 1.0 };
            for s in f {
                bytes.extend_from_slice(&(s * gain * fade).to_le_bytes());
            }
        }
        fade_in = fade_in.saturating_sub(CHUNK);
        // The tiny pipe makes this block until the player wants more: that's the pacing.
        if tx.write_all(&bytes).await.is_err() {
            return;
        }
    }
}

impl Engine {
    /// Reconciles the running delay lines with `routes`. Delay and gain changes
    /// apply live without interrupting the stream; only new or vanished routes
    /// start or stop.
    pub async fn set_routes(&self, routes: &[Route]) -> Result<()> {
        self.reap_stale().await;
        if let Some(r) = routes.iter().find(|r| r.sink == SINK) {
            bail!("can't route {} into itself", r.sink);
        }
        let mut st = self.inner.state.lock().await;
        let wanted: HashSet<&str> = routes.iter().map(|r| r.sink.as_str()).collect();
        st.routes.retain(|sink, _| wanted.contains(sink.as_str()));
        let mut first_err = None;
        for route in routes {
            if let Some(a) = st.routes.get_mut(&route.sink) {
                let alive = matches!(a.play.try_wait(), Ok(None)) && matches!(a.rec.try_wait(), Ok(None));
                if alive {
                    a.ctl.target_frames.store(target_for(route.delay_ms), Ordering::Relaxed);
                    a.ctl.gain.store(gain_lin(route.gain_db).to_bits(), Ordering::Relaxed);
                    a.delay_ms = route.delay_ms;
                    a.gain_db = route.gain_db;
                    continue;
                }
                st.routes.remove(&route.sink);
            }
            match start(route).await {
                Ok(a) => {
                    st.routes.insert(route.sink.clone(), a);
                }
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    pub async fn clear_routes(&self) {
        let routes = std::mem::take(&mut self.inner.state.lock().await.routes);
        for (_, mut a) in routes {
            let _ = a.play.kill().await;
            let _ = a.rec.kill().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_names() {
        assert_eq!(route_name("bluez_output.80_4A_F2_94_D2_5A.1"), "sonance-route-80_4A_F2_94_D2_5A");
        assert_eq!(route_name("sonance-test"), "sonance-route-sonance_test");
    }

    #[test]
    fn control_loop() {
        let t = target_for(200.0);
        assert_eq!(decide(0, t), Step::Wait);
        assert_eq!(decide(t, t), Step::Steady);
        assert_eq!(decide(t + 100, t), Step::Nudge(1));
        assert_eq!(decide(t - 100, t), Step::Nudge(-1));
        assert_eq!(decide(t + 5000, t), Step::Jump(5000));
        // Zero delay still keeps a cushion bigger than one recorder burst.
        assert!(target_for(0.0) - NUDGE_LIMIT > CHUNK as i64);
    }
}

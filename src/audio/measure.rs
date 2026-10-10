//! Microphone calibration: play a sweep into a sink, hear it back, time and level it.
//!
//! The recorder takes two channels: the microphone, and the sink's own monitor as a
//! sample-accurate reference for when the sweep reached PipeWire's sink. Both arrive through
//! one stream, so process start-up jitter cancels out. Only if the sink has no monitor ports do
//! we fall back to wall-clock timestamps of the two processes.

use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

use super::{Engine, dsp, eq, pw};

#[derive(Debug, Clone, PartialEq)]
pub struct Measurement {
    pub latency_ms: f32,
    /// dBFS RMS of the direct sound.
    pub level_db: f32,
    /// Peak-to-noise of the correlation, 0..1-ish.
    pub confidence: f32,
    /// Response in the octave bands of [`eq::BANDS_HZ`], relative to the 500 Hz–2 kHz mean;
    /// NaN where the band was lost in noise.
    pub bands_db: Vec<f32>,
}

const RATE: u32 = 48_000;
const PAD_MS: u32 = 100;
const MIN_CONFIDENCE: f32 = 0.4;
/// The sweep spans every EQ band with room to spare at both ends; 1.5 s keeps it short to sit
/// through while giving the bass enough energy to clear room noise.
pub(super) const SWEEP_S: f32 = 1.5;
pub(super) const SWEEP_HZ: (f32, f32) = (40.0, 16_000.0);
pub(super) const SWEEP_DB: f32 = -12.0;

fn ms(n: u32) -> usize {
    (n as u64 * RATE as u64 / 1000) as usize
}

/// The first output port of `node` that is (or isn't) a monitor port.
fn port_of<'a>(ports: &'a [String], node: &str, monitor: bool) -> Option<&'a str> {
    ports.iter().map(String::as_str).find(|p| {
        p.strip_prefix(node)
            .and_then(|r| r.strip_prefix(':'))
            .is_some_and(|port| port.starts_with("monitor_") == monitor)
    })
}

/// Reads interleaved stereo f32 from the recorder. Returns the samples and the wall-clock
/// instant that corresponds to frame 0 (the earliest bound over all reads).
pub(super) async fn read_recorder(
    mut out: tokio::process::ChildStdout,
    need: watch::Receiver<usize>,
    progress: watch::Sender<usize>,
) -> (Vec<f32>, Option<Instant>) {
    let mut samples = Vec::new();
    let mut origin: Option<Instant> = None;
    let mut buf = vec![0u8; 16384];
    let mut rest = Vec::new();
    loop {
        let n = match out.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let now = Instant::now();
        rest.extend_from_slice(&buf[..n]);
        let whole = rest.len() / 4 * 4;
        samples.extend(rest[..whole].as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)));
        rest.drain(..whole);
        let frames = samples.len() / 2;
        let t = now.checked_sub(Duration::from_secs_f64(frames as f64 / RATE as f64)).unwrap_or(now);
        origin = Some(origin.map_or(t, |o| o.min(t)));
        let _ = progress.send(frames);
        if frames >= *need.borrow() {
            break;
        }
    }
    (samples, origin)
}

pub(super) async fn wait_frames(rx: &mut watch::Receiver<usize>, frames: usize, timeout: Duration) -> bool {
    tokio::time::timeout(timeout, rx.wait_for(|&f| f >= frames)).await.is_ok_and(|r| r.is_ok())
}

impl Engine {
    /// Audio inputs, as (node name, description).
    pub async fn mics(&self) -> Result<Vec<(String, String)>> {
        Ok(pw::nodes()
            .await?
            .into_iter()
            .filter(|n| matches!(n.class(), "Audio/Source" | "Audio/Source/Virtual"))
            .map(|n| {
                let desc = n.prop("node.description").or(n.prop("node.nick")).unwrap_or(n.name()).to_string();
                (n.name().to_string(), desc)
            })
            .collect())
    }

    /// Plays quiet pink noise into `sink` for `secs`. A Sonos speaker only
    /// switches to its Bluetooth input once it hears real sound, ~2.5 s after
    /// it starts, so a test sweep sent cold is never played; this wakes it.
    pub async fn wake(&self, sink: &str, secs: f32) -> Result<()> {
        let cmd = format!(
            "ffmpeg -loglevel error -f lavfi -i 'anoisesrc=color=pink:duration={secs}:amplitude=0.08' -f wav - | pw-play --target '{sink}' -"
        );
        let status = pw::command("sh").arg("-c").arg(cmd).status().await.context("couldn't play the wake-up noise")?;
        if !status.success() {
            bail!("couldn't play into {sink}");
        }
        Ok(())
    }

    /// Plays a 1.5 s exponential sweep (40 Hz–16 kHz, -12 dBFS) into `sink` while recording `mic`,
    /// and returns how long after reaching the sink it was heard, how loud, and its frequency response.
    /// `mic` may also be a sink, in which case its monitor is recorded (handy for testing).
    pub async fn measure(&self, sink: &str, mic: &str, max_latency_ms: u32) -> Result<Measurement> {
        self.reap_stale().await;
        let _one_at_a_time = self.inner.measuring.lock().await;
        let nodes = pw::nodes().await?;
        if !nodes.iter().any(|n| n.name() == sink && n.class().starts_with("Audio/Sink")) {
            bail!("output {sink} isn't available");
        }
        let mic_node = nodes.iter().find(|n| n.name() == mic).with_context(|| format!("microphone {mic} isn't available"))?;
        let outs = pw::ports(true).await?;
        let mic_port = port_of(&outs, mic, mic_node.class().starts_with("Audio/Sink"))
            .with_context(|| format!("microphone {mic} has no capture ports"))?
            .to_string();
        let ref_port = port_of(&outs, sink, true).map(String::from);

        let rec_name = format!("sonance-measure-rec-{}", std::process::id());
        let mut rec = pw::command("pw-cat")
            .args(["--record", "--raw", "--rate", "48000", "--channels", "2", "--channel-map", "FL,FR"])
            .args(["--format", "f32", "--latency", "256", "--target", "0", "-P"])
            .arg(format!("{{ node.name={rec_name} node.autoconnect=false node.dont-fallback=true }}"))
            .arg("-")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("couldn't start pw-cat")?;
        let stdout = rec.stdout.take().context("recorder stdout")?;
        let (need_tx, need_rx) = watch::channel(usize::MAX);
        let (prog_tx, mut prog_rx) = watch::channel(0usize);
        let reader = tokio::spawn(read_recorder(stdout, need_rx, prog_tx));

        let deadline = Instant::now() + Duration::from_secs(3);
        let (rec_l, rec_r) = (format!("{rec_name}:input_FL"), format!("{rec_name}:input_FR"));
        while !pw::ports(false).await?.contains(&rec_r) {
            if Instant::now() > deadline {
                bail!("the recorder didn't start");
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        pw::run("pw-link", &[&mic_port, &rec_l]).await?;
        if let Some(r) = &ref_port {
            pw::run("pw-link", &[r, &rec_r]).await?;
        }
        // Let the graph settle so the recording is flowing before anything is played.
        if !wait_frames(&mut prog_rx, ms(300), Duration::from_secs(3)).await {
            bail!("microphone {mic} isn't delivering audio");
        }

        let sweep = dsp::sweep(RATE, SWEEP_S, SWEEP_HZ.0, SWEEP_HZ.1, SWEEP_DB, 0.01);
        let mut signal = vec![0f32; ms(PAD_MS)];
        signal.extend_from_slice(&sweep);
        signal.extend(std::iter::repeat_n(0.0, ms(200)));
        let bytes: Vec<u8> = signal.iter().flat_map(|v| v.to_le_bytes()).collect();
        let played_at = *prog_rx.borrow();
        let need = played_at + signal.len() + ms(max_latency_ms) + ms(1000);
        let _ = need_tx.send(need);
        let mut play = pw::command("pw-cat")
            .args(["--playback", "--raw", "--rate", "48000", "--channels", "1", "--format", "f32"])
            .args(["--latency", "256", "--target", sink, "-P"])
            .arg("{ node.name=sonance-measure-play node.dont-fallback=true node.dont-reconnect=true state.restore-props=false }")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("couldn't start pw-cat")?;
        let t_play = Instant::now();
        let mut stdin = play.stdin.take().context("player stdin")?;
        let writer = tokio::spawn(async move {
            let _ = stdin.write_all(&bytes).await;
        });

        let budget = Duration::from_millis((PAD_MS + (SWEEP_S * 1000.0) as u32 + 200 + max_latency_ms + 3000) as u64);
        let heard_all = wait_frames(&mut prog_rx, need, budget).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), play.wait()).await;
        let _ = play.kill().await;
        writer.abort();
        let _ = rec.kill().await;
        let (samples, origin) = reader.await.context("recorder reader")?;
        if !heard_all {
            bail!("the recording stalled");
        }

        let mic_ch: Vec<f32> = samples.iter().step_by(2).copied().collect();
        let ref_ch: Vec<f32> = samples.iter().skip(1).step_by(2).copied().collect();
        let reference = ref_port
            .is_some()
            .then(|| dsp::find_peak(&dsp::xcorr(&ref_ch, &sweep), played_at..ref_ch.len()))
            .flatten()
            .filter(|p| dsp::confidence(p.ratio) >= 0.5)
            .map(|p| p.lag);
        let t0 = match (reference, origin) {
            (Some(lag), _) => lag,
            (None, Some(o)) => t_play.saturating_duration_since(o).as_secs_f32() * RATE as f32 + ms(PAD_MS) as f32,
            (None, None) => bail!("nothing was recorded"),
        };
        let from = (t0 - ms(5) as f32).max(0.0) as usize;
        let to = t0 as usize + ms(max_latency_ms) + ms(50);
        let peak = dsp::find_peak(&dsp::xcorr(&mic_ch, &sweep), from..to).context("recording too short")?;
        let start = peak.lag.round() as usize;
        let level_db = dsp::rms_dbfs(&mic_ch[start.min(mic_ch.len())..(start + sweep.len()).min(mic_ch.len())]);
        let confidence = dsp::confidence(peak.ratio);
        if confidence < MIN_CONFIDENCE || level_db < -90.0 {
            bail!("couldn't hear the test tone (confidence {confidence:.2}, level {level_db:.0} dBFS)");
        }
        let bands_db = eq::relative_to_mid(&dsp::band_response(&mic_ch, start, &sweep, RATE, &eq::BANDS_HZ));
        Ok(Measurement { latency_ms: (peak.lag - t0) / RATE as f32 * 1000.0, level_db, confidence, bands_db })
    }
}

#[cfg(test)]
mod tests {
    use super::port_of;

    #[test]
    fn picks_ports() {
        let ports: Vec<String> = ["sonance-test:monitor_FL", "sonance:monitor_FL", "mic:capture_MONO", "mic2:monitor_FL"]
            .map(String::from)
            .into();
        assert_eq!(port_of(&ports, "sonance", true), Some("sonance:monitor_FL"));
        assert_eq!(port_of(&ports, "mic", false), Some("mic:capture_MONO"));
        assert_eq!(port_of(&ports, "mic", true), None);
    }
}

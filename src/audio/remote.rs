//! Calibration with a microphone that isn't on this PC (a phone, through a
//! web page). The phone records the whole session as one long take; the PC
//! notes, for each speaker, the moment its sweep reached that speaker's
//! output. Lining the two clocks up is only approximate (it comes from when
//! chunks of the take arrived over the network), so it just narrows down
//! where to look; the arrival itself comes from the correlation.
//!
//! The offset between the two clocks is the same for every speaker, so the
//! latencies are exact relative to each other but share an unknown constant;
//! the caller anchors them.

use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

use super::measure::{Measurement, read_recorder, wait_frames};
use super::{Engine, dsp, eq, pw};

const RATE: u32 = 48_000;
const PAD_MS: u32 = 100;
const MIN_CONFIDENCE: f32 = 0.35;
/// Slack around where a sweep is expected in the phone's take: covers the
/// network-derived clock estimate and the phone's own buffering.
const SLACK_S: f32 = 0.6;

/// The same sweep [`Engine::measure`] uses, at any sample rate.
pub fn sweep_at(rate: u32) -> Vec<f32> {
    dsp::sweep(rate, super::measure::SWEEP_S, super::measure::SWEEP_HZ.0, super::measure::SWEEP_HZ.1, super::measure::SWEEP_DB, 0.01)
}

impl Engine {
    /// Plays the test sweep into `sink` and returns when it reached the sink, on this PC's clock,
    /// found in the sink's own monitor (so process start-up delays don't count).
    pub async fn play_marked(&self, sink: &str) -> Result<Instant> {
        self.reap_stale().await;
        let _one_at_a_time = self.inner.measuring.lock().await;
        let outs = pw::ports(true).await?;
        let monitor = outs
            .iter()
            .find(|p| p.strip_prefix(sink).and_then(|r| r.strip_prefix(':')).is_some_and(|port| port.starts_with("monitor_")))
            .with_context(|| format!("output {sink} has no monitor"))?
            .clone();

        let rec_name = format!("sonance-mark-rec-{}", std::process::id());
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
        let rec_l = format!("{rec_name}:input_FL");
        while !pw::ports(false).await?.contains(&rec_l) {
            if Instant::now() > deadline {
                bail!("the recorder didn't start");
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        pw::run("pw-link", &[&monitor, &rec_l]).await?;
        if !wait_frames(&mut prog_rx, RATE as usize * 3 / 10, Duration::from_secs(3)).await {
            bail!("{sink} isn't running");
        }

        let sweep = sweep_at(RATE);
        let mut signal = vec![0f32; (PAD_MS * RATE / 1000) as usize];
        signal.extend_from_slice(&sweep);
        signal.extend(std::iter::repeat_n(0.0, RATE as usize / 5));
        let bytes: Vec<u8> = signal.iter().flat_map(|v| v.to_le_bytes()).collect();
        let played_at = *prog_rx.borrow();
        let need = played_at + signal.len() + RATE as usize;
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
        let mut stdin = play.stdin.take().context("player stdin")?;
        let writer = tokio::spawn(async move {
            let _ = stdin.write_all(&bytes).await;
        });
        let heard = wait_frames(&mut prog_rx, need, Duration::from_secs(8)).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), play.wait()).await;
        let _ = play.kill().await;
        writer.abort();
        let _ = rec.kill().await;
        let (samples, origin) = reader.await.context("recorder reader")?;
        if !heard {
            bail!("the recording stalled");
        }
        let mono: Vec<f32> = samples.iter().step_by(2).copied().collect();
        let peak = dsp::find_peak(&dsp::xcorr(&mono, &sweep), played_at..mono.len())
            .filter(|p| dsp::confidence(p.ratio) >= 0.5)
            .context("the sweep never reached the output")?;
        let origin = origin.context("nothing was recorded")?;
        Ok(origin + Duration::from_secs_f64(peak.lag as f64 / RATE as f64))
    }
}

/// Finds the sweep that reached its output at `sent` in the phone's take.
/// `origin` is the PC instant matching sample 0 of `take` (approximately).
/// Latencies come out offset by the clock error, the same for every call on one take.
pub fn analyse(take: &[f32], rate: u32, origin: Instant, sent: Instant, max_latency_ms: u32) -> Result<Measurement> {
    let sweep = sweep_at(rate);
    let at = sent.saturating_duration_since(origin).as_secs_f32();
    let from = ((at - SLACK_S).max(0.0) * rate as f32) as usize;
    let to = ((at + max_latency_ms as f32 / 1000.0 + SLACK_S) * rate as f32) as usize;
    if from >= take.len() {
        bail!("the phone's recording stops before this speaker played");
    }
    let end = (to + sweep.len()).min(take.len());
    let seg = &take[from..end];
    let peak = dsp::find_peak(&dsp::xcorr(seg, &sweep), 0..(to - from).min(seg.len())).context("recording too short")?;
    let start = from + peak.lag.round() as usize;
    let level_db = dsp::rms_dbfs(&take[start.min(take.len())..(start + sweep.len()).min(take.len())]);
    let confidence = dsp::confidence(peak.ratio);
    if confidence < MIN_CONFIDENCE || level_db < -90.0 {
        bail!("couldn't hear the test tone (confidence {confidence:.2}, level {level_db:.0} dBFS)");
    }
    let bands_db = eq::relative_to_mid(&dsp::band_response(take, start, &sweep, rate, &eq::BANDS_HZ));
    let latency_ms = ((from as f32 + peak.lag) / rate as f32 - at) * 1000.0;
    Ok(Measurement { latency_ms, level_db, confidence, bands_db })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_sweeps_in_a_long_take() {
        // A 44.1 kHz "phone" take with two sweeps, 1.2 s and 4.5 s in, and a clock that is
        // 80 ms off; latencies 300 and 340 ms relative to when each was sent.
        let rate = 44_100;
        let sweep = sweep_at(rate);
        let mut take = vec![0f32; rate as usize * 9];
        let mut seed = 1u32;
        for v in take.iter_mut() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *v = ((seed >> 9) as f32 / (1u32 << 23) as f32 - 0.5) * 0.01;
        }
        for (at, gain) in [(1.2f32, 0.3f32), (4.5, 0.15)] {
            let i = (at * rate as f32) as usize;
            for (k, s) in sweep.iter().enumerate() {
                take[i + k] += s * gain;
            }
        }
        let origin = Instant::now();
        let clock_err = 0.08;
        let sent = |t: f32| origin + Duration::from_secs_f32(t - clock_err);
        let a = analyse(&take, rate, origin, sent(1.2 - 0.3), 1500).unwrap();
        let b = analyse(&take, rate, origin, sent(4.5 - 0.34), 1500).unwrap();
        assert!((b.latency_ms - a.latency_ms - 40.0).abs() < 1.0, "{} {}", a.latency_ms, b.latency_ms);
        assert!((a.level_db - b.level_db - 6.0).abs() < 0.5);
    }
}

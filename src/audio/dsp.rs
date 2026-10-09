//! The pure maths behind calibration: test sweep, matched filter, level, octave-band response.

use std::ops::Range;

use rustfft::FftPlanner;
use rustfft::num_complex::Complex32;

/// Exponential sine sweep from `f1` to `f2` Hz over `secs`, at `level_db` dBFS peak, with
/// raised-cosine fades of `fade_s` so the edges don't click.
pub fn sweep(rate: u32, secs: f32, f1: f32, f2: f32, level_db: f32, fade_s: f32) -> Vec<f32> {
    let n = (secs * rate as f32) as usize;
    let fade = ((fade_s * rate as f32) as usize).max(1).min(n / 2);
    let amp = 10f32.powf(level_db / 20.0);
    let k = (f2 / f1).ln();
    let t_total = secs as f64;
    (0..n)
        .map(|i| {
            let t = i as f64 / rate as f64;
            let phase = 2.0 * std::f64::consts::PI * f1 as f64 * t_total / k as f64 * ((t / t_total * k as f64).exp() - 1.0);
            let edge = i.min(n - 1 - i);
            let env = if edge < fade { 0.5 - 0.5 * (std::f32::consts::PI * edge as f32 / fade as f32).cos() } else { 1.0 };
            amp * env * phase.sin() as f32
        })
        .collect()
}

/// Cross-correlation `c[k] = Σ x[n+k]·s[n]` for every non-negative lag `k < x.len()`, via FFT.
pub fn xcorr(x: &[f32], s: &[f32]) -> Vec<f32> {
    if x.is_empty() || s.is_empty() {
        return vec![0.0; x.len()];
    }
    let n = (x.len() + s.len()).next_power_of_two();
    let mut planner = FftPlanner::<f32>::new();
    let fwd = planner.plan_fft_forward(n);
    let inv = planner.plan_fft_inverse(n);
    let pad = |v: &[f32]| {
        let mut b: Vec<Complex32> = v.iter().map(|&r| Complex32::new(r, 0.0)).collect();
        b.resize(n, Complex32::default());
        b
    };
    let (mut a, mut b) = (pad(x), pad(s));
    fwd.process(&mut a);
    fwd.process(&mut b);
    for (p, q) in a.iter_mut().zip(&b) {
        *p *= q.conj();
    }
    inv.process(&mut a);
    a.iter().take(x.len()).map(|c| c.re / n as f32).collect()
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Peak {
    /// Lag in samples, refined below one sample by parabolic interpolation.
    pub lag: f32,
    /// Peak |c| over the median |c| in the searched range.
    pub ratio: f32,
}

/// Strongest |c| within `range` (polarity doesn't matter: a speaker may be wired inverted).
pub fn find_peak(c: &[f32], range: Range<usize>) -> Option<Peak> {
    let range = range.start.min(c.len())..range.end.min(c.len());
    if range.len() < 3 {
        return None;
    }
    let mag: Vec<f32> = c[range.clone()].iter().map(|v| v.abs()).collect();
    let (i, &peak) = mag.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1))?;
    let mut sorted = mag.clone();
    let mid = sorted.len() / 2;
    let median = *sorted.select_nth_unstable_by(mid, f32::total_cmp).1;
    let frac = if i > 0 && i + 1 < mag.len() {
        let (l, m, r) = (mag[i - 1], mag[i], mag[i + 1]);
        let d = l - 2.0 * m + r;
        if d.abs() > f32::EPSILON { (0.5 * (l - r) / d).clamp(-0.5, 0.5) } else { 0.0 }
    } else {
        0.0
    };
    Some(Peak { lag: (range.start + i) as f32 + frac, ratio: if median > 0.0 { peak / median } else if peak > 0.0 { f32::INFINITY } else { 0.0 } })
}

/// Maps a peak-to-median ratio onto roughly 0..1. Pure noise lands around a ratio of 6–8
/// (the largest of ~10⁵ Gaussian samples over their median magnitude), hence the offset.
pub fn confidence(ratio: f32) -> f32 {
    (1.0 - 8.0 / ratio).clamp(0.0, 1.0)
}

/// RMS level in dBFS (a full-scale square wave is 0 dBFS), floored at -120.
pub fn rms_dbfs(x: &[f32]) -> f32 {
    if x.is_empty() {
        return -120.0;
    }
    let ms = x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len() as f64;
    (10.0 * ms.log10()).max(-120.0) as f32
}

/// Impulse-response window for band levels: long enough to hold the direct sound, the early
/// reflections and the room modes that colour what's heard, short enough to leave out the
/// reverberant tail and most of the noise.
const IR_WINDOW_S: f32 = 0.2;
/// Window starts this far ahead of the detected arrival, in case the matched-filter peak lands a
/// little after the onset (a speaker with little bass rings in late).
const IR_PRE_S: f32 = 0.004;
/// Where an equally long window of the impulse response holds only noise: past any plausible
/// room decay, well before the distortion products, which deconvolve to negative times.
const NOISE_AT_S: f32 = 0.45;
const MIN_SNR_DB: f32 = 10.0;

fn fft(planner: &mut FftPlanner<f32>, x: &[f32], n: usize) -> Vec<Complex32> {
    let mut b: Vec<Complex32> = x.iter().map(|&r| Complex32::new(r, 0.0)).collect();
    b.resize(n, Complex32::default());
    planner.plan_fft_forward(n).process(&mut b);
    b
}

/// Octave-band response (dB, absolute: a wire reads 0) of whatever turned `sweep` into `rec`,
/// where the sweep arrives at sample `start`. NaN where the band is within `MIN_SNR_DB` of the
/// noise floor.
///
/// The recording is deconvolved by the sweep into an impulse response, which is then windowed:
/// the reverberant tail falls outside the window, and the harmonic distortion of an exponential
/// sweep lands before it. The noise floor is measured from a later stretch of the same impulse
/// response with the same window, so both go through identical processing.
pub fn band_response(rec: &[f32], start: usize, sweep: &[f32], rate: u32, centres: &[f32]) -> Vec<f32> {
    let secs = |s: f32| (s * rate as f32) as usize;
    let seg_start = start.saturating_sub(secs(IR_PRE_S));
    let onset = start - seg_start;
    let win = onset + secs(IR_WINDOW_S);
    let noise_at = onset + secs(NOISE_AT_S);
    let seg = &rec[seg_start.min(rec.len())..(seg_start + noise_at + win + sweep.len()).min(rec.len())];
    if seg.is_empty() || sweep.is_empty() || centres.is_empty() {
        return vec![f32::NAN; centres.len()];
    }

    let n = (noise_at + win + 2 * sweep.len()).next_power_of_two();
    let mut planner = FftPlanner::<f32>::new();
    let mut h = fft(&mut planner, seg, n);
    let s = fft(&mut planner, sweep, n);
    let hz = |bin: usize| bin as f32 * rate as f32 / n as f32;
    let lo = centres.iter().copied().fold(f32::INFINITY, f32::min) / std::f32::consts::SQRT_2;
    let hi = centres.iter().copied().fold(0.0, f32::max) * std::f32::consts::SQRT_2;
    // Regularised against the sweep's in-band power, so bins it never covered come out near zero
    // instead of as amplified noise. Above the bands there's also a gentle taper; below them
    // there's none, as its acausal ringing would fall outside the window and cost the bass band.
    let mut power: Vec<f32> = (0..n / 2).filter(|&b| (lo..hi).contains(&hz(b))).map(|b| s[b].norm_sqr()).collect();
    let mid = power.len() / 2;
    let eps = 1e-3 * power.select_nth_unstable_by(mid, f32::total_cmp).1.max(f32::MIN_POSITIVE);
    for (b, (x, s)) in h.iter_mut().zip(&s).enumerate() {
        let f = hz(b.min(n - b));
        let taper = if f > hi * 1.5 {
            0.0
        } else if f > hi {
            0.5 + 0.5 * (std::f32::consts::PI * (f - hi) / (hi / 2.0)).cos()
        } else {
            1.0
        };
        *x = *x * s.conj() / (s.norm_sqr() + eps) * (taper / n as f32);
    }
    planner.plan_fft_inverse(n).process(&mut h);

    // Fade in over the pre-roll's first half, out over the last quarter.
    let window = |at: usize| -> Vec<f32> {
        let (rise, fall) = ((onset / 2).max(1), (win / 4).max(1));
        (0..win)
            .map(|i| {
                let w = if i < rise {
                    0.5 - 0.5 * (std::f32::consts::PI * i as f32 / rise as f32).cos()
                } else if i >= win - fall {
                    0.5 + 0.5 * (std::f32::consts::PI * (i - (win - fall)) as f32 / fall as f32).cos()
                } else {
                    1.0
                };
                h.get(at + i).map_or(0.0, |c| c.re * w)
            })
            .collect()
    };
    let m = (win * 4).next_power_of_two();
    let spectrum = |ir: Vec<f32>, planner: &mut FftPlanner<f32>| fft(planner, &ir, m).iter().map(|c| c.norm_sqr()).collect::<Vec<f32>>();
    let direct = spectrum(window(0), &mut planner);
    let noise = spectrum(window(noise_at), &mut planner);
    let bin = |f: f32| ((f * m as f32 / rate as f32).round() as usize).min(m / 2);
    centres
        .iter()
        .map(|&c| {
            let bins = bin(c / std::f32::consts::SQRT_2)..bin(c * std::f32::consts::SQRT_2).max(bin(c / std::f32::consts::SQRT_2) + 1);
            let k = bins.len() as f64;
            let p = direct[bins.clone()].iter().map(|&v| v as f64).sum::<f64>() / k;
            let q = noise[bins].iter().map(|&v| v as f64).sum::<f64>() / k;
            if p > q * 10f64.powf(MIN_SNR_DB as f64 / 10.0) { (10.0 * (p - q).log10()) as f32 } else { f32::NAN }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::eq::{BANDS_HZ, Biquad};

    const RATE: u32 = 48_000;

    /// Deterministic white-ish noise in [-amp, amp] (xorshift), so tests are reproducible.
    fn noise(n: usize, amp: f32, mut seed: u64) -> Vec<f32> {
        (0..n)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                amp * ((seed >> 40) as f32 / (1u64 << 23) as f32 - 1.0)
            })
            .collect()
    }

    #[test]
    fn sweep_shape() {
        let s = sweep(RATE, 1.0, 100.0, 10_000.0, -12.0, 0.01);
        assert_eq!(s.len(), 48_000);
        assert!(s[0].abs() < 1e-3 && s[s.len() - 1].abs() < 1e-2);
        let peak = s.iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!((peak - 10f32.powf(-12.0 / 20.0)).abs() < 0.01);
    }

    #[test]
    fn finds_delayed_sweep_in_noise() {
        let s = sweep(RATE, 1.0, 100.0, 10_000.0, -12.0, 0.01);
        for (delay, gain, nz) in [(12_345usize, 0.3f32, 0.05f32), (150_000, 0.05, 0.05), (7, 1.0, 0.0)] {
            let mut x = noise(delay + s.len() + 96_000, nz, delay as u64 + 1);
            for (i, v) in s.iter().enumerate() {
                x[delay + i] += gain * v;
            }
            let c = xcorr(&x, &s);
            let p = find_peak(&c, 0..c.len()).unwrap();
            let err_ms = (p.lag - delay as f32).abs() / RATE as f32 * 1000.0;
            assert!(err_ms < 1.0, "delay {delay}: found {}", p.lag);
            assert!(confidence(p.ratio) > 0.5, "delay {delay}: ratio {}", p.ratio);
        }
    }

    #[test]
    fn noise_alone_is_not_confident() {
        let s = sweep(RATE, 1.0, 100.0, 10_000.0, -12.0, 0.01);
        let x = noise(4 * RATE as usize, 0.1, 42);
        let c = xcorr(&x, &s);
        let p = find_peak(&c, 0..c.len()).unwrap();
        assert!(confidence(p.ratio) < 0.4, "ratio {}", p.ratio);
    }

    #[test]
    fn inverted_polarity_is_found() {
        let s = sweep(RATE, 1.0, 100.0, 10_000.0, -12.0, 0.01);
        let mut x = vec![0.0; 2000];
        x.extend(s.iter().map(|v| -v));
        let p = find_peak(&xcorr(&x, &s), 0..x.len()).unwrap();
        assert!((p.lag - 2000.0).abs() < 1.0);
    }

    #[test]
    fn levels() {
        assert!((rms_dbfs(&[0.5; 100]) - (-6.02)).abs() < 0.01);
        let sine: Vec<f32> = (0..48_000).map(|i| (i as f32 * 0.1).sin()).collect();
        assert!((rms_dbfs(&sine) - (-3.01)).abs() < 0.05);
        assert_eq!(rms_dbfs(&[0.0; 10]), -120.0);
    }

    /// The production sweep, run through `filters` and delayed into noise, against the filters'
    /// own power response averaged over each octave.
    fn recovers(filters: &[Biquad], nz: f32) -> (Vec<f32>, Vec<f32>) {
        let rate = 48_000.0;
        let s = sweep(RATE, 1.5, 40.0, 16_000.0, -12.0, 0.01);
        let delay = 20_000;
        let mut x = noise(delay + s.len() + RATE as usize, nz, 7);
        let mut state = vec![[0.0; 2]; filters.len()];
        for (i, &v) in s.iter().chain(std::iter::repeat_n(&0.0, RATE as usize / 2)).enumerate() {
            let mut y = v as f64;
            for (b, st) in filters.iter().zip(&mut state) {
                y = b.run(y, st);
            }
            if let Some(o) = x.get_mut(delay + i) {
                *o += y as f32;
            }
        }
        let got = band_response(&x, delay, &s, RATE, &BANDS_HZ);
        let want = BANDS_HZ
            .iter()
            .map(|&c| {
                let fs: Vec<f64> = (0..200).map(|i| c as f64 / 2f64.sqrt() * 2f64.powf(i as f64 / 200.0)).collect();
                // Bins are linear in frequency, so weight each sample by its bandwidth.
                let (p, w) = fs.iter().fold((0.0, 0.0), |(p, w), &f| {
                    let db: f64 = filters.iter().map(|b| b.response_db(rate, f)).sum();
                    (p + 10f64.powf(db / 10.0) * f, w + f)
                });
                (10.0 * (p / w).log10()) as f32
            })
            .collect();
        (got, want)
    }

    #[test]
    fn band_response_of_a_wire() {
        let (got, _) = recovers(&[], 0.0);
        assert!(got.iter().all(|v| v.abs() < 0.1), "{got:?}");
    }

    #[test]
    fn band_response_finds_known_bumps() {
        for (f0, db) in [(63.0, 6.0), (250.0, 6.0), (1000.0, -8.0), (4000.0, 6.0), (8000.0, -6.0)] {
            let filters = [Biquad::peaking(48_000.0, f0, std::f64::consts::SQRT_2, db)];
            let (got, want) = recovers(&filters, 0.02);
            for ((g, w), f) in got.iter().zip(&want).zip(BANDS_HZ) {
                assert!((g - w).abs() < 1.5, "{db} dB at {f0} Hz: band {f} got {g:.2}, want {w:.2}\n{got:?}");
            }
            let peak = BANDS_HZ.iter().position(|&f| f as f64 == f0).unwrap();
            assert!(got[peak].abs() > 3.0, "{f0}: {got:?}");
        }
    }

    #[test]
    fn band_response_survives_a_room() {
        // A bass resonance and a treble roll-off together, plus a strong reflection 7 ms late.
        let filters = [Biquad::peaking(48_000.0, 125.0, 4.0, 9.0), Biquad::peaking(48_000.0, 8000.0, 0.7, -9.0)];
        let (direct, want) = recovers(&filters, 0.02);
        let s = sweep(RATE, 1.5, 40.0, 16_000.0, -12.0, 0.01);
        let mut x = noise(20_000 + s.len() + RATE as usize, 0.02, 3);
        for (i, v) in s.iter().enumerate() {
            x[20_000 + i] += v;
            x[20_000 + 336 + i] += 0.5 * v;
        }
        let echo = band_response(&x, 20_000, &s, RATE, &BANDS_HZ);
        for (g, c) in echo.iter().zip(BANDS_HZ) {
            let fs = (0..400).map(|i| c / 2f32.sqrt() * 2f32.powf(i as f32 / 400.0));
            let (p, w) = fs.fold((0.0, 0.0), |(p, w), f| {
                (p + (1.25 + (std::f32::consts::TAU * f * 336.0 / 48_000.0).cos()) * f, w + f)
            });
            let want = 10.0 * (p / w).log10();
            assert!((g - want).abs() < 1.5, "echo, band {c}: got {g:.2}, want {want:.2}");
        }
        for (g, w) in direct.iter().zip(&want) {
            assert!((g - w).abs() < 1.5, "{direct:?} vs {want:?}");
        }
    }

    #[test]
    fn band_response_reports_noise_as_unknown() {
        // Nearly nothing left above 1 kHz: those bands are below the noise and must say so.
        let lp = |q| {
            let w0 = std::f64::consts::TAU * 700.0 / 48_000.0;
            let alpha = w0.sin() / (2.0 * q);
            let a0 = 1.0 + alpha;
            Biquad::from_coeffs(
                (1.0 - w0.cos()) / 2.0 / a0,
                (1.0 - w0.cos()) / a0,
                (1.0 - w0.cos()) / 2.0 / a0,
                -2.0 * w0.cos() / a0,
                (1.0 - alpha) / a0,
            )
        };
        let (got, want) = recovers(&[lp(0.54), lp(1.31), lp(0.54), lp(1.31)], 0.02);
        assert!(got[..4].iter().all(|v| v.is_finite()), "{got:?}");
        assert!(got[6..].iter().all(|v| v.is_nan()), "{got:?} vs {want:?}");
        assert!((got[2] - want[2]).abs() < 1.5, "{got:?} vs {want:?}");
    }

    #[test]
    fn silence_is_not_confident() {
        let p = find_peak(&[0.0; 100], 0..100).unwrap();
        assert_eq!(confidence(p.ratio), 0.0);
    }
}

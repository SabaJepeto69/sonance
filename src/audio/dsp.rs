//! The pure maths behind calibration: test sweep, matched filter, level.

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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn silence_is_not_confident() {
        let p = find_peak(&[0.0; 100], 0..100).unwrap();
        assert_eq!(confidence(p.ratio), 0.0);
    }
}

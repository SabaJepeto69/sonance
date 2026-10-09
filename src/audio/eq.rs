//! Room correction: what to change given a measured response, and the filters that change it.
//!
//! Everything works on octave bands at [`BANDS_HZ`]. A Bluetooth route gets the full per-band
//! correction as a cascade of peaking filters; a Sonos speaker only has bass and treble, so the
//! correction is folded onto those.

/// Centres of the octave bands every response and correction is expressed in.
pub const BANDS_HZ: [f32; 8] = [63., 125., 250., 500., 1000., 2000., 4000., 8000.];

/// The response we aim for, relative to the 500 Hz–2 kHz mean: a gentle downward tilt, which
/// rooms sound natural with, rather than flat (which sounds thin and bright).
const HOUSE_DB: [f32; 8] = [2.0, 1.5, 1.0, 0.5, 0.0, -0.5, -1.25, -2.0];
const MAX_CUT_DB: f32 = -10.0;
/// Boosting into a dip mostly wastes headroom (dips are often cancellations no gain can fill),
/// and consumer microphones are least trustworthy exactly where dips show up.
const MAX_BOOST_DB: f32 = 3.0;
const MAX_LOW_BOOST_DB: f32 = 1.5;
/// Sonos doesn't document its tone controls. Measured on a Move: four bass
/// steps moved 63 Hz by 3.3 dB, six treble steps moved 8 kHz by 4.7 dB.
pub const DB_PER_SONOS_STEP: f32 = 0.8;
/// Peaking-filter Q for octave-spaced bands (bandwidth of one octave).
const Q: f64 = std::f64::consts::SQRT_2;

fn is_mid(f: f32) -> bool {
    (500.0..=2000.0).contains(&f)
}

/// Mean of the finite values among `bands` (indexed like [`BANDS_HZ`]) whose centre passes `pick`.
fn mean_where(bands: &[f32], pick: impl Fn(f32) -> bool) -> Option<f32> {
    let v: Vec<f32> = bands.iter().zip(BANDS_HZ).filter(|(v, f)| v.is_finite() && pick(*f)).map(|(v, _)| *v).collect();
    (!v.is_empty()).then(|| v.iter().sum::<f32>() / v.len() as f32)
}

/// Shifts a band response so the 500 Hz–2 kHz mean is 0 dB. All NaN if none of those is known.
pub(super) fn relative_to_mid(bands_db: &[f32]) -> Vec<f32> {
    let mid = mean_where(bands_db, is_mid).unwrap_or(f32::NAN);
    bands_db.iter().map(|v| v - mid).collect()
}

/// Per-band gains (dB) that move `bands_db` (relative to mid, NaN = unknown) toward the house
/// curve, scaled by `strength` (0..1). Unknown bands get 0.
pub fn correction(bands_db: &[f32], strength: f32) -> Vec<f32> {
    let raw: Vec<Option<f32>> = bands_db
        .iter()
        .enumerate()
        .map(|(i, v)| v.is_finite().then(|| HOUSE_DB.get(i).copied().unwrap_or(0.0) - v))
        .collect();
    let strength = strength.clamp(0.0, 1.0);
    (0..raw.len())
        .map(|i| {
            let Some(own) = raw[i] else { return 0.0 };
            // A 1-2-1 blend with whichever neighbours are known, so a single narrow ripple
            // isn't chased at full depth.
            let (mut sum, mut w) = (2.0 * own, 2.0);
            for j in [i.wrapping_sub(1), i + 1] {
                if let Some(Some(n)) = raw.get(j) {
                    sum += n;
                    w += 1.0;
                }
            }
            let max = if BANDS_HZ.get(i).is_some_and(|&f| f < 90.0) { MAX_LOW_BOOST_DB } else { MAX_BOOST_DB };
            (sum / w).clamp(MAX_CUT_DB, max) * strength
        })
        .collect()
}

/// Folds a per-band correction onto Sonos bass and treble steps (each -10..=10).
pub fn sonos_tone(correction_db: &[f32]) -> (i32, i32) {
    let mid = mean_where(correction_db, is_mid).unwrap_or(0.0);
    let at = |hz: f32| BANDS_HZ.iter().position(|&f| f == hz).and_then(|i| correction_db.get(i)).copied().filter(|v| v.is_finite());
    // Sonos's bass control is a low shelf: 125 Hz is the heart of it, 63 Hz the band most
    // speakers barely reproduce, 250 Hz its upper edge.
    let weighted = |parts: &[(f32, f32)]| {
        let (s, w) = parts.iter().filter_map(|&(hz, w)| at(hz).map(|v| (v * w, w))).fold((0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1));
        if w > 0.0 { s / w - mid } else { 0.0 }
    };
    let bass = weighted(&[(63.0, 0.25), (125.0, 0.45), (250.0, 0.3)]);
    let treble = weighted(&[(4000.0, 0.5), (8000.0, 0.5)]);
    let steps = |db: f32| ((db / DB_PER_SONOS_STEP).round() as i32).clamp(-10, 10);
    (steps(bass), steps(treble))
}

/// One second-order section, RBJ cookbook, normalised so a0 = 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl Biquad {
    pub fn peaking(rate: f64, f0: f64, q: f64, db: f64) -> Self {
        let a = 10f64.powf(db / 40.0);
        let w0 = std::f64::consts::TAU * f0 / rate;
        let alpha = w0.sin() / (2.0 * q);
        let a0 = 1.0 + alpha / a;
        Self {
            b0: (1.0 + alpha * a) / a0,
            b1: -2.0 * w0.cos() / a0,
            b2: (1.0 - alpha * a) / a0,
            a1: -2.0 * w0.cos() / a0,
            a2: (1.0 - alpha / a) / a0,
        }
    }

    #[cfg(test)]
    pub fn from_coeffs(b0: f64, b1: f64, b2: f64, a1: f64, a2: f64) -> Self {
        Self { b0, b1, b2, a1, a2 }
    }

    /// Magnitude response at `f` Hz, in dB.
    pub fn response_db(&self, rate: f64, f: f64) -> f64 {
        let w = std::f64::consts::TAU * f / rate;
        let (c1, s1, c2, s2) = (w.cos(), -w.sin(), (2.0 * w).cos(), -(2.0 * w).sin());
        let num = ((self.b0 + self.b1 * c1 + self.b2 * c2).powi(2) + (self.b1 * s1 + self.b2 * s2).powi(2)).sqrt();
        let den = ((1.0 + self.a1 * c1 + self.a2 * c2).powi(2) + (self.a1 * s1 + self.a2 * s2).powi(2)).sqrt();
        20.0 * (num / den).log10()
    }

    /// Transposed direct form II; `s` is this section's state for one channel.
    #[inline]
    pub fn run(&self, x: f64, s: &mut [f64; 2]) -> f64 {
        let y = self.b0 * x + s[0];
        s[0] = self.b1 * x - self.a1 * y + s[1];
        s[1] = self.b2 * x - self.a2 * y;
        y
    }
}

/// Per-band peaking filters on interleaved stereo, with state kept across coefficient changes
/// so a live EQ change doesn't click.
pub struct Equalizer {
    rate: f64,
    filters: Vec<Biquad>,
    state: Vec<[[f64; 2]; 2]>,
}

impl Equalizer {
    pub fn new(rate: u32) -> Self {
        Self { rate: rate as f64, filters: Vec::new(), state: Vec::new() }
    }

    pub fn is_flat(&self) -> bool {
        self.filters.is_empty()
    }

    /// Sets the gain of each band in [`BANDS_HZ`] (missing or NaN = 0 dB).
    pub fn set(&mut self, eq_db: &[f32]) {
        let want: Vec<f64> = (0..BANDS_HZ.len()).map(|i| eq_db.get(i).copied().filter(|v| v.is_finite()).unwrap_or(0.0) as f64).collect();
        if want.iter().all(|g| g.abs() < 0.01) {
            self.filters.clear();
            self.state.clear();
            return;
        }
        // Octave-wide peaks overlap, so a run of equal gains would stack up past what was asked.
        // A few Jacobi steps on the gains make the cascade hit the requested level at each centre.
        let mut gains = want.clone();
        for _ in 0..8 {
            let filters = self.design(&gains);
            for (i, f) in BANDS_HZ.iter().enumerate() {
                let got: f64 = filters.iter().map(|b| b.response_db(self.rate, *f as f64)).sum();
                gains[i] = (gains[i] + want[i] - got).clamp(-24.0, 24.0);
            }
        }
        self.filters = self.design(&gains);
        self.state.resize(self.filters.len(), [[0.0; 2]; 2]);
    }

    fn design(&self, gains: &[f64]) -> Vec<Biquad> {
        BANDS_HZ.iter().zip(gains).map(|(&f, &g)| Biquad::peaking(self.rate, f as f64, Q, g)).collect()
    }

    pub fn process(&mut self, frames: &mut [[f32; 2]]) {
        for fr in frames {
            for (c, v) in fr.iter_mut().enumerate() {
                let mut x = *v as f64;
                for (b, s) in self.filters.iter().zip(&mut self.state) {
                    x = b.run(x, &mut s[c]);
                }
                *v = x as f32;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 48_000.0;

    /// Measured gain of `eq` at `f` Hz: RMS out over RMS in of a steady sine, after settling.
    fn sine_gain_db(eq: &mut Equalizer, f: f64) -> f64 {
        let n = (RATE * 0.5) as usize;
        let mut frames: Vec<[f32; 2]> =
            (0..n).map(|i| [(std::f64::consts::TAU * f * i as f64 / RATE).sin() as f32; 2]).collect();
        let input: f64 = frames[n / 2..].iter().map(|x| (x[0] as f64).powi(2)).sum();
        eq.process(&mut frames);
        let output: f64 = frames[n / 2..].iter().map(|x| (x[1] as f64).powi(2)).sum();
        10.0 * (output / input).log10()
    }

    #[test]
    fn peaking_filter_shape() {
        for (f0, db) in [(63.0, 6.0), (1000.0, -8.0), (8000.0, 3.0)] {
            let b = Biquad::peaking(RATE, f0, Q, db);
            assert!((b.response_db(RATE, f0) - db).abs() < 0.5, "{f0} Hz: {}", b.response_db(RATE, f0));
            for far in [f0 / 4.0, f0 * 4.0] {
                if far < 20_000.0 {
                    assert!(b.response_db(RATE, far).abs() < 1.0, "{f0} Hz at {far}: {}", b.response_db(RATE, far));
                }
            }
        }
    }

    #[test]
    fn equalizer_hits_band_gains() {
        let want = [3.0, -6.0, 0.0, 2.0, 2.0, 2.0, -10.0, 1.0];
        let mut eq = Equalizer::new(48_000);
        eq.set(&want);
        for (f, w) in BANDS_HZ.iter().zip(want) {
            let got = sine_gain_db(&mut eq, *f as f64);
            assert!((got - w as f64).abs() < 0.5, "{f} Hz: wanted {w}, got {got:.2}");
        }
    }

    #[test]
    fn flat_eq_is_skipped() {
        let mut eq = Equalizer::new(48_000);
        eq.set(&[]);
        assert!(eq.is_flat());
        eq.set(&[0.0, f32::NAN, 0.0]);
        assert!(eq.is_flat());
        eq.set(&[0.0, 1.0]);
        assert!(!eq.is_flat());
        eq.set(&[0.0; 8]);
        assert!(eq.is_flat());
    }

    #[test]
    fn mid_normalisation() {
        let r = relative_to_mid(&[5.0, 4.0, 3.0, 2.0, 3.0, 4.0, f32::NAN, 0.0]);
        assert_eq!(&r[..6], &[2.0, 1.0, 0.0, -1.0, 0.0, 1.0]);
        assert!(r[6].is_nan());
        assert!(relative_to_mid(&[0.0, 0.0, 0.0, f32::NAN, f32::NAN, f32::NAN, 0.0, 0.0]).iter().all(|v| v.is_nan()));
    }

    #[test]
    fn correction_follows_house_curve() {
        assert_eq!(correction(&HOUSE_DB, 1.0), vec![0.0; 8]);
        // A uniform 2 dB excess is cut by exactly 2 dB everywhere.
        let loud: Vec<f32> = HOUSE_DB.iter().map(|v| v + 2.0).collect();
        assert!(correction(&loud, 1.0).iter().all(|c| (c + 2.0).abs() < 1e-5));
    }

    #[test]
    fn correction_limits() {
        let c = correction(&[-20.0; 8], 1.0);
        assert_eq!(c[0], MAX_LOW_BOOST_DB);
        assert!(c[1..].iter().all(|&v| v == MAX_BOOST_DB));
        assert!(correction(&[30.0; 8], 1.0).iter().all(|&v| v == MAX_CUT_DB));
    }

    #[test]
    fn correction_smooths_ripples() {
        let mut bands = HOUSE_DB;
        bands[4] += 6.0;
        let c = correction(&bands, 1.0);
        assert!((c[4] + 3.0).abs() < 1e-5, "{c:?}");
        assert!((c[3] + 1.5).abs() < 1e-5 && (c[5] + 1.5).abs() < 1e-5, "{c:?}");
        assert!(c[0] == 0.0 && c[7] == 0.0);
    }

    #[test]
    fn correction_nan_and_strength() {
        let mut bands = HOUSE_DB.map(|v| v + 4.0);
        bands[0] = f32::NAN;
        bands[6] = f32::NAN;
        let full = correction(&bands, 1.0);
        assert_eq!(full[0], 0.0);
        assert_eq!(full[6], 0.0);
        assert!(full.iter().all(|v| v.is_finite()));
        // Neighbours of an unknown band aren't dragged toward zero by it.
        assert!((full[5] + 4.0).abs() < 1e-5 && (full[1] + 4.0).abs() < 1e-5, "{full:?}");
        let half = correction(&bands, 0.5);
        for (h, f) in half.iter().zip(&full) {
            assert!((h - f / 2.0).abs() < 1e-6);
        }
        assert!(correction(&bands, 0.0).iter().all(|&v| v == 0.0));
        assert_eq!(correction(&bands, 7.0), full);
    }

    #[test]
    fn tone_mapping() {
        assert_eq!(sonos_tone(&[0.0; 8]), (0, 0));
        assert_eq!(sonos_tone(&[]), (0, 0));
        assert_eq!(sonos_tone(&[3.0, 3.0, 3.0, 0.0, 0.0, 0.0, -3.0, -3.0]), (4, -4));
        // Relative to the mids: lifting everything changes nothing.
        assert_eq!(sonos_tone(&[2.0; 8]), (0, 0));
        assert_eq!(sonos_tone(&[0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.5, 1.5]), (0, 2));
        assert_eq!(sonos_tone(&[-40.0, -40.0, -40.0, 0.0, 0.0, 0.0, 40.0, 40.0]), (-10, 10));
        // Bass weighs 125 Hz most.
        assert_eq!(sonos_tone(&[0.0, -6.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]), (-3, 0));
    }
}

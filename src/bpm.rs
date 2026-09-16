//! Where a tempo comes from, and where it is remembered.
//!
//! Force Tempo needs one number per track. It comes from the file's `TBPM` tag
//! if it has one; otherwise the audio is measured. Either way the answer is
//! **remembered per track**, so it is worked out once and never again.
//!
//! # Three states, not two
//!
//! "Nothing known yet" and "nothing there" are deliberately different, all the
//! way out to the API:
//!
//! * **absent** — not looked at yet.
//! * **[`Bpm::None`]** — analysed, and found to have no steady tempo. An
//!   audiobook, a drone, a field recording. It plays unchanged and is **never
//!   analysed again**.
//! * **[`Bpm::Known`]** — a number, plus where it came from.
//!
//! Collapsing the middle state into "absent" would mean re-measuring a podcast
//! every time it came round; collapsing it into a number would mean a player
//! that invented a tempo for a podcast and then played it 30% fast, which is
//! indefensible.
//!
//! # The measurement is editable
//!
//! Because it is a measurement rather than a preference, and you can hear when
//! it is wrong. A number the user supplies is [`Source::User`], **wins**, and is
//! never quietly re-measured; clearing it throws the measurement away and has
//! another go.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::silence::{self, RATE};

// ---- the store ---------------------------------------------------------

/// Where a tempo came from. Kept because it decides what may overwrite it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The file's own `TBPM` / `BPM` tag. Trusted over a measurement — whoever
    /// tagged it had the whole track and, usually, the sleeve.
    Tag,
    /// Measured from the audio by [`detect`].
    Measured,
    /// Typed in by the user. Never overwritten by anything else.
    User,
}

/// What is known about one track's tempo.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Bpm {
    /// Analysed, and there is no steady tempo in it.
    None,
    Known { bpm: f64, source: Source },
}

impl Bpm {
    pub fn value(&self) -> Option<f64> {
        match self {
            Bpm::Known { bpm, .. } => Some(*bpm),
            Bpm::None => None,
        }
    }

    pub fn source(&self) -> Option<Source> {
        match self {
            Bpm::Known { source, .. } => Some(*source),
            Bpm::None => None,
        }
    }

    /// A user-supplied number is final: it is not re-measured, and a later
    /// analysis must not quietly replace it.
    pub fn is_pinned(&self) -> bool {
        self.source() == Some(Source::User)
    }
}

/// Per-track tempos, persisted to `~/.config/gapless/bpm.json`.
///
/// A separate file from `state.json` for the same reason `ratings.json` is one:
/// `state.json` is rewritten every few seconds while playing, and this is a
/// cache that took a decode per track to build. It is keyed by absolute path,
/// not queue index, because a rescan renumbers the queue.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct BpmStore {
    #[serde(default)]
    tracks: HashMap<PathBuf, Bpm>,
}

fn store_path() -> Option<PathBuf> {
    let dir = glib::user_config_dir().join("gapless");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("bpm.json"))
}

impl BpmStore {
    pub fn load() -> Self {
        let Some(path) = store_path() else {
            return Self::default();
        };
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Written through a temporary file and renamed. Losing this file costs a
    /// decode per track to rebuild, so a crash mid-write must not truncate it.
    pub fn save(&self) {
        let Some(path) = store_path() else { return };
        let Ok(json) = serde_json::to_string_pretty(self) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, json) {
            eprintln!("could not save tempos: {e}");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            eprintln!("could not save tempos: {e}");
            let _ = std::fs::remove_file(&tmp);
        }
    }

    pub fn get(&self, path: &Path) -> Option<Bpm> {
        self.tracks.get(path).copied()
    }

    /// True when this track has never been looked at — the only state that
    /// warrants spending a decode on it.
    pub fn needs_analysis(&self, path: &Path) -> bool {
        !self.tracks.contains_key(path)
    }

    /// Record an analysis result. Refuses to overwrite a user-supplied number,
    /// so an analysis already in flight when the user types cannot land on top
    /// of what they typed.
    pub fn record(&mut self, path: &Path, bpm: Bpm) {
        if self.get(path).map(|b| b.is_pinned()).unwrap_or(false) {
            return;
        }
        self.tracks.insert(path.to_path_buf(), bpm);
    }

    /// Type a number over the measurement. `None` clears the entry entirely, so
    /// the track is measured again from scratch — "I don't know either, have
    /// another go" rather than "there is no tempo here".
    pub fn set_user(&mut self, path: &Path, bpm: Option<f64>) {
        match bpm {
            Some(v) if v.is_finite() && v > 0.0 => {
                self.tracks
                    .insert(path.to_path_buf(), Bpm::Known { bpm: v, source: Source::User });
            }
            _ => {
                self.tracks.remove(path);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }
}

// ---- reading the tag ---------------------------------------------------

/// The file's own tempo tag, if it has a believable one.
///
/// `TBPM` is an integer frame in ID3 and a free string in Vorbis, and both are
/// full of zeroes and junk from taggers that write the field whether or not they
/// know anything. Anything outside a plausible musical range is treated as an
/// absent tag rather than as a tempo.
pub fn from_tag(path: &Path) -> Option<f64> {
    use lofty::file::TaggedFileExt;
    use lofty::prelude::ItemKey;
    use lofty::probe::Probe;

    let tagged = Probe::open(path).ok()?.read().ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let raw = tag.get_string(&ItemKey::Bpm)?;
    let value: f64 = raw.trim().parse().ok()?;
    plausible(value).then_some(value)
}

/// The range a written-down tempo has to fall in to be believed at all. Wider
/// than the range Force Tempo will *target*, because a tag saying 210 is a real
/// tempo that simply cannot be a target; a tag saying 0 or 6000 is a broken
/// tagger.
pub fn plausible(bpm: f64) -> bool {
    bpm.is_finite() && (30.0..=300.0).contains(&bpm)
}

// ---- measuring it ------------------------------------------------------

/// Envelope resolution. 10 ms is fine enough to place an onset and coarse enough
/// that a 30 s window is 3,000 points rather than 240,000.
const FRAME: usize = (RATE as usize) / 100;

/// Skip the opening of a track — it is the least representative part of it, and
/// an intro at half time is exactly how an octave error gets made.
const SKIP_SECS: usize = 30;
/// How much audio to measure. Half a minute is several dozen bars at any tempo
/// in range.
const WINDOW_SECS: usize = 30;
/// Below this there is not enough to autocorrelate meaningfully.
const MIN_WINDOW_SECS: usize = 10;

/// The tempo range searched. Wider than the *target* range: a 200 BPM track is
/// a real thing to find even though nothing will be stretched to 210.
const MIN_BPM: f64 = 60.0;
const MAX_BPM: f64 = 200.0;

/// Lag resolution, in envelope frames. **A quarter of a frame, not a whole one**
/// — and that is not a precision nicety, it is the difference between a right
/// answer and an octave. A 160 BPM beat lands every 37.5 frames, so *neither*
/// lag 37 nor lag 38 lines the rhythm up with itself, while lag 75 — two beats —
/// lines it up perfectly. A whole-frame search reports 80 BPM for that track,
/// confidently.
const LAG_STEP: f64 = 0.25;

/// How far the winning lag has to stand above the rest of the search before we
/// call it a tempo. Below this the track is reported as having none, which is a
/// real answer and not a failure — see the module docs.
///
/// **Measured, not chosen by taste.** `examples/bpm-info.rs` exists to produce
/// these numbers. On a 14-track sample of real music plus the synthetic
/// fixtures:
///
/// | | confidence |
/// |---|---|
/// | white noise | 0.068 |
/// | a 42 s spoken comedy interlude | 0.088 |
/// | the weakest real song in the sample | 0.134 |
/// | the strongest | 0.420 |
/// | a clean synthetic click track | 0.93–0.99 |
///
/// 0.10 sits in the gap, and lands the spoken track on the "no tempo" side —
/// which is what it is. The margin either side is narrow enough that this is
/// worth re-measuring rather than nudged blind if it ever misfires.
const MIN_CONFIDENCE: f64 = 0.10;

/// Where the preference curve is centred, and how wide it is in octaves. Every
/// tempo estimator needs one: correlation alone cannot choose between a tempo
/// and its double, because a rhythm that repeats every beat also repeats every
/// two. This is the standard log-normal perceptual weighting, centred on the
/// pace people actually clap at.
const PREFERRED_BPM: f64 = 120.0;
const PREFERENCE_OCTAVES: f64 = 0.9;

/// How nearly as good the faster reading has to be to win the tie-break.
///
/// A rhythm that repeats every beat also repeats every two beats, so the slower
/// lag *always* correlates at least as well — picking the maximum alone would
/// systematically halve every tempo. But the converse does not hold: if the beat
/// really were the slower one, the half-lag would be lining beats up with the
/// gaps between them and correlating badly. So the half-lag holding up to within
/// 10% is not a coincidence, and the faster reading is taken.
const OCTAVE_TIE: f64 = 0.90;

/// What the estimator found, including how sure it is. The tempo alone is what
/// playback needs; the rest is what `examples/bpm-info.rs` prints, and is how
/// [`MIN_CONFIDENCE`] was chosen rather than guessed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    /// `None` means "analysed, no steady tempo" — a real answer.
    pub bpm: Option<f64>,
    /// How far the winning lag stands above the rest of the search, 0.0..=1.0.
    pub confidence: f64,
    /// The tempo before the octave tie-break, for diagnosing a halved answer.
    pub raw_bpm: Option<f64>,
}

/// Measure a track's tempo. `Ok(None)` means "analysed, no steady tempo" — a
/// real answer, distinct from an error.
pub fn detect(path: &Path) -> Result<Option<f64>> {
    Ok(analyze(path)?.bpm)
}

pub fn analyze(path: &Path) -> Result<Reading> {
    let samples = silence::decode_mono_8k(path)?;
    Ok(analyze_samples(&samples))
}

/// The measurement proper, on decoded 8 kHz mono. Split out from the decode so
/// it can be tested on synthesised audio with an exactly known answer.
pub fn detect_samples(samples: &[f32]) -> Option<f64> {
    analyze_samples(samples).bpm
}

pub fn analyze_samples(samples: &[f32]) -> Reading {
    let nothing = Reading { bpm: None, confidence: 0.0, raw_bpm: None };
    let Some(window) = representative_window(samples) else {
        return nothing;
    };
    let envelope = onset_envelope(window);
    best_tempo(&envelope)
}

/// The stretch of a track to measure: 30 s starting 30 s in, where that exists.
///
/// Where it does not, the start is pulled back far enough to keep a **full**
/// window rather than kept at 30 s and the window truncated. Skipping the intro
/// is a preference; having enough audio to autocorrelate is a requirement, and
/// trading the second for the first is how a 42 s track ends up measured on 12
/// seconds and reported as having no tempo. A track shorter than a full window
/// starts a tenth of the way in, which still skips a fade-in without throwing
/// away most of what there is.
fn representative_window(samples: &[f32]) -> Option<&[f32]> {
    let min_window = MIN_WINDOW_SECS * RATE as usize;
    if samples.len() < min_window {
        return None;
    }
    let skip = SKIP_SECS * RATE as usize;
    let want = WINDOW_SECS * RATE as usize;
    let start = if samples.len() > want {
        skip.min(samples.len() - want)
    } else {
        samples.len() / 10
    };
    let end = (start + want).min(samples.len());
    Some(&samples[start..end])
}

/// An onset envelope: how much louder each 10 ms frame is than the one before
/// it, in log amplitude, half-wave rectified.
///
/// Rectified because an onset is an *increase*. A decay is not a beat, and
/// letting it contribute negatively would make the envelope track the shape of
/// the note rather than the moment it started.
fn onset_envelope(samples: &[f32]) -> Vec<f64> {
    let frames = samples.len() / FRAME;
    let mut energy = Vec::with_capacity(frames);
    for f in 0..frames {
        let chunk = &samples[f * FRAME..(f + 1) * FRAME];
        let sum: f64 = chunk.iter().map(|v| (*v as f64) * (*v as f64)).sum();
        // Log of the RMS, floored well below anything audible so that digital
        // silence is a finite number rather than -inf.
        energy.push((sum / FRAME as f64).sqrt().max(1e-9).ln());
    }

    let mut flux = Vec::with_capacity(frames.saturating_sub(1));
    for i in 1..energy.len() {
        flux.push((energy[i] - energy[i - 1]).max(0.0));
    }

    // Band-limit it before anything interpolates it, which [`correlate`] does at
    // every fractional lag. Without this the whole quarter-frame search is a
    // fiction: an onset landing exactly on a frame boundary and one landing
    // half-way between two frames produce differently-shaped spikes, so
    // correlating them gives a low answer *because of the framing* rather than
    // because the rhythm disagrees.
    //
    // It shows up as a systematic octave error on precisely the tempos the
    // quarter-frame grid was added for. A 160 BPM beat is 37.5 frames, so
    // alternate beats land on alternate phases and the sharp envelope genuinely
    // repeats every 75 frames — the estimator then reports 80 BPM, confidently,
    // which is the exact failure the fine grid was supposed to prevent.
    let flux = smooth(&flux);

    // Centre it: autocorrelation of a signal with a large DC component measures
    // the DC, which is the same at every lag and tells you nothing.
    let mean = flux.iter().sum::<f64>() / flux.len().max(1) as f64;
    flux.into_iter().map(|v| v - mean).collect()
}

/// A 5-tap binomial blur, about 20 ms wide. Narrow enough to leave a beat at
/// 200 BPM (a 30-frame period) perfectly distinct, wide enough that the envelope
/// can be interpolated between frames without lying.
fn smooth(flux: &[f64]) -> Vec<f64> {
    const K: [f64; 5] = [1.0, 4.0, 6.0, 4.0, 1.0];
    const NORM: f64 = 16.0;
    let n = flux.len();
    (0..n)
        .map(|i| {
            let mut acc = 0.0;
            let mut weight = 0.0;
            for (k, w) in K.iter().enumerate() {
                let off = i as isize + k as isize - 2;
                if off >= 0 && (off as usize) < n {
                    acc += flux[off as usize] * w;
                    weight += w;
                }
            }
            // Renormalised at the edges rather than zero-padded, so the first and
            // last frames are not artificially quiet.
            if weight > 0.0 { acc / weight } else { 0.0 * NORM }
        })
        .collect()
}

/// Pick the lag whose (weighted) self-similarity is strongest, resolve its
/// octave, and turn it into a tempo.
fn best_tempo(envelope: &[f64]) -> Reading {
    let nothing = Reading { bpm: None, confidence: 0.0, raw_bpm: None };
    if envelope.len() < 4 * (60.0 / MIN_BPM * 100.0) as usize {
        return nothing; // fewer than four of the slowest beats: nothing to measure
    }

    let lag_for = |bpm: f64| 60.0 / bpm * 100.0; // frames, at 10 ms each
    let bpm_for = |lag: f64| 60.0 / (lag / 100.0);

    let min_lag = lag_for(MAX_BPM);
    let max_lag = lag_for(MIN_BPM);

    let mut best_lag = 0.0;
    let mut best_score = f64::MIN;
    let mut correlations: Vec<f64> = Vec::new();
    let mut lag = min_lag;
    while lag <= max_lag {
        let r = correlate(envelope, lag);
        correlations.push(r);
        let score = r * preference(bpm_for(lag));
        if score > best_score {
            best_score = score;
            best_lag = lag;
        }
        lag += LAG_STEP;
    }

    if best_lag <= 0.0 || correlations.is_empty() {
        return nothing;
    }

    // Confidence is how far the winning lag stands ABOVE THE REST OF THE SEARCH,
    // not its bare correlation.
    //
    // A bare correlation cannot tell a beat from a coincidence. Rectified onset
    // flux is not white — it has structure at every lag — so the largest of
    // ~280 candidates sits well above zero even for noise, and a fixed
    // "r > 0.1" gate duly reports a confident tempo for a field recording. What
    // distinguishes a real beat is that ONE lag is much better than its
    // neighbours. Measured on the fixtures: a click track clears 0.5 and both
    // noise and digital silence stay under 0.1.
    //
    // Judged on the unweighted correlations, too: the preference curve exists to
    // choose between octaves, and letting it also decide whether there is a beat
    // at all would make a tempo more believable merely for being near 120.
    let peak = correlate(envelope, best_lag);
    let mean = correlations.iter().sum::<f64>() / correlations.len() as f64;
    let confidence = if peak.is_finite() { (peak - mean).max(0.0) } else { 0.0 };

    let raw_bpm = Some(bpm_for(best_lag));
    if confidence < MIN_CONFIDENCE {
        return Reading { bpm: None, confidence, raw_bpm };
    }

    // Rounded to a tenth. The lag grid is quarter-frame, so the underlying
    // resolution is a few tenths of a BPM at best — publishing
    // `84.50704225352113` through the API would be seventeen digits of precision
    // for a number that is accurate to about one.
    let bpm = (bpm_for(resolve_octave(envelope, best_lag, min_lag)) * 10.0).round() / 10.0;
    Reading { bpm: Some(bpm), confidence, raw_bpm }
}

/// Take the faster reading when the audio supports it nearly as well. See
/// [`OCTAVE_TIE`] for why "nearly as well" is the right test and "better" is
/// not. Applied repeatedly, so a quarter-time reading climbs all the way back.
fn resolve_octave(envelope: &[f64], lag: f64, min_lag: f64) -> f64 {
    let mut lag = lag;
    loop {
        let half = lag / 2.0;
        if half < min_lag {
            return lag;
        }
        let here = correlate(envelope, lag);
        let faster = correlate(envelope, half);
        if faster >= here * OCTAVE_TIE {
            lag = half;
        } else {
            return lag;
        }
    }
}

/// Normalised autocorrelation at a possibly-fractional lag, in −1.0..=1.0.
///
/// Normalised over the overlapping region specifically, so that long lags — which
/// have less overlap to work with — are not penalised into irrelevance by a
/// denominator computed over the whole signal.
fn correlate(envelope: &[f64], lag: f64) -> f64 {
    let n = envelope.len();
    let whole = lag.floor() as usize;
    let frac = lag - whole as f64;
    if whole + 1 >= n {
        return 0.0;
    }
    let count = n - whole - 1;

    let mut num = 0.0;
    let mut den_a = 0.0;
    let mut den_b = 0.0;
    for i in 0..count {
        let a = envelope[i];
        // Linear interpolation between the two neighbouring frames. This is what
        // makes a quarter-frame lag mean anything at all.
        let b = envelope[i + whole] * (1.0 - frac) + envelope[i + whole + 1] * frac;
        num += a * b;
        den_a += a * a;
        den_b += b * b;
    }

    let den = (den_a * den_b).sqrt();
    if den <= 0.0 {
        0.0
    } else {
        num / den
    }
}

/// The perceptual preference curve: a log-normal centred on [`PREFERRED_BPM`].
fn preference(bpm: f64) -> f64 {
    let octaves = (bpm / PREFERRED_BPM).log2();
    (-0.5 * (octaves / PREFERENCE_OCTAVES).powi(2)).exp()
}

/// `128` / `128.5` — a tempo as the UI and the API show it. Whole numbers stay
/// whole, because "128.0 BPM" reads as more precision than a measurement has.
pub fn format_bpm(bpm: f64) -> String {
    if (bpm - bpm.round()).abs() < 0.05 {
        format!("{:.0}", bpm.round())
    } else {
        format!("{bpm:.1}")
    }
}

/// Turn a typed-in string into a tempo, refusing anything implausible rather
/// than clamping it into a lie.
pub fn parse_bpm(text: &str) -> Result<Option<f64>> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None); // clear it and measure again
    }
    let v: f64 = t.parse().map_err(|_| anyhow!("not a number: {t}"))?;
    if !plausible(v) {
        return Err(anyhow!("{t} is not a plausible tempo (30-300 BPM)"));
    }
    Ok(Some(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A click track at a known tempo: one short burst per beat, silence between.
    fn clicks(bpm: f64, secs: f64) -> Vec<f32> {
        let rate = RATE as f64;
        let n = (secs * rate) as usize;
        let period = 60.0 / bpm * rate;
        let mut out = vec![0.0f32; n];
        let mut beat = 0.0;
        while (beat as usize) < n {
            let at = beat as usize;
            // 15 ms of decaying tone — an onset with a shape, not a single spike.
            for i in 0..(rate * 0.015) as usize {
                if at + i >= n {
                    break;
                }
                let t = i as f64 / rate;
                let env = (-t * 120.0).exp();
                out[at + i] += (env * (2.0 * std::f64::consts::PI * 900.0 * t).sin()) as f32;
            }
            beat += period;
        }
        out
    }

    fn detected(bpm: f64) -> f64 {
        detect_samples(&clicks(bpm, 45.0)).unwrap_or_else(|| panic!("no tempo found at {bpm}"))
    }

    /// Within a quarter of a percent — the lag grid is quantised, so an exact
    /// equality would be testing the grid rather than the estimator.
    fn close(found: f64, want: f64) {
        let err = (found - want).abs() / want;
        assert!(err < 0.02, "found {found:.2} BPM, wanted {want:.2} ({:.1}% out)", err * 100.0);
    }

    #[test]
    fn finds_a_plain_tempo() {
        close(detected(120.0), 120.0);
        close(detected(128.0), 128.0);
        close(detected(90.0), 90.0);
    }

    /// The case [`LAG_STEP`] exists for. A 160 BPM beat lands every 37.5 frames;
    /// a whole-frame search reports 80 and is confident about it.
    #[test]
    fn a_tempo_between_two_whole_frames_is_not_halved() {
        close(detected(160.0), 160.0);
    }

    #[test]
    fn finds_a_fast_tempo_without_halving_it() {
        close(detected(175.0), 175.0);
        close(detected(190.0), 190.0);
    }

    #[test]
    fn finds_a_slow_tempo_without_doubling_it() {
        close(detected(70.0), 70.0);
    }

    /// Noise has no beat in it, and saying so is the right answer rather than a
    /// failure. This is the state an audiobook or a field recording lands in.
    #[test]
    fn noise_has_no_tempo() {
        // A deterministic LCG, so this test cannot pass or fail by luck.
        let mut seed = 0x2545F4914F6CDD1Du64;
        let noise: Vec<f32> = (0..(RATE as usize * 45))
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((seed >> 33) as f64 / (1u64 << 31) as f64 - 1.0) as f32 * 0.3
            })
            .collect();
        let r = analyze_samples(&noise);
        println!("noise confidence {:.3} (threshold {MIN_CONFIDENCE})", r.confidence);
        assert_eq!(r.bpm, None, "noise must report no tempo");
        assert!(
            r.confidence < MIN_CONFIDENCE,
            "noise scored {:.3}, at or above the {MIN_CONFIDENCE} threshold",
            r.confidence
        );
    }

    /// The other side of that gate: real rhythmic audio must clear the threshold
    /// with room to spare, or the first quietly-mixed track will fall through it.
    #[test]
    fn a_real_beat_clears_the_threshold_with_margin() {
        for bpm in [90.0, 120.0, 160.0] {
            let r = analyze_samples(&clicks(bpm, 45.0));
            println!("{bpm} BPM confidence {:.3}", r.confidence);
            assert!(
                r.confidence > MIN_CONFIDENCE * 2.0,
                "a clean beat at {bpm} scored only {:.3}",
                r.confidence
            );
        }
    }

    #[test]
    fn digital_silence_has_no_tempo() {
        assert_eq!(detect_samples(&vec![0.0; RATE as usize * 45]), None);
    }

    #[test]
    fn a_clip_too_short_to_measure_reports_nothing_rather_than_guessing() {
        assert_eq!(detect_samples(&clicks(120.0, 3.0)), None);
    }

    // ---- the store ----

    #[test]
    fn three_states_survive_a_round_trip() {
        let mut s = BpmStore::default();
        let measured = Path::new("/music/01 measured.flac");
        let none = Path::new("/music/02 audiobook.m4a");
        let unknown = Path::new("/music/03 never looked at.mp3");

        s.record(measured, Bpm::Known { bpm: 128.0, source: Source::Measured });
        s.record(none, Bpm::None);

        let back: BpmStore = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.get(measured).unwrap().value(), Some(128.0));
        assert_eq!(back.get(none), Some(Bpm::None));
        assert_eq!(back.get(unknown), None);

        // The distinction that matters: one has been looked at, one has not.
        assert!(!back.needs_analysis(none), "a track with no tempo must never be re-measured");
        assert!(back.needs_analysis(unknown));
    }

    #[test]
    fn a_user_supplied_tempo_is_never_overwritten_by_an_analysis() {
        let mut s = BpmStore::default();
        let p = Path::new("/music/the detector got this one wrong.flac");
        s.set_user(p, Some(85.0));
        // An analysis already in flight lands afterwards and must be ignored.
        s.record(p, Bpm::Known { bpm: 170.0, source: Source::Measured });
        assert_eq!(s.get(p).unwrap().value(), Some(85.0));
        s.record(p, Bpm::None);
        assert_eq!(s.get(p).unwrap().value(), Some(85.0));
    }

    /// Clearing must mean "measure it again", not "there is no tempo here" —
    /// those are different states and the user picking the first must not land
    /// them in the second, which is never revisited.
    #[test]
    fn clearing_a_user_tempo_restores_it_to_unmeasured() {
        let mut s = BpmStore::default();
        let p = Path::new("/music/have another go.flac");
        s.set_user(p, Some(85.0));
        s.set_user(p, None);
        assert_eq!(s.get(p), None);
        assert!(s.needs_analysis(p));
    }

    #[test]
    fn an_implausible_typed_tempo_is_refused_rather_than_clamped() {
        assert!(parse_bpm("0").is_err());
        assert!(parse_bpm("6000").is_err());
        assert!(parse_bpm("fast").is_err());
        assert_eq!(parse_bpm("").unwrap(), None);
        assert_eq!(parse_bpm(" 128 ").unwrap(), Some(128.0));
        assert_eq!(parse_bpm("128.5").unwrap(), Some(128.5));
    }

    #[test]
    fn a_junk_tag_is_not_a_tempo() {
        assert!(!plausible(0.0));
        assert!(!plausible(f64::NAN));
        assert!(!plausible(6000.0));
        assert!(plausible(210.0), "210 is a real tempo even though nothing targets it");
    }

    #[test]
    fn formats_without_inventing_precision() {
        assert_eq!(format_bpm(128.0), "128");
        assert_eq!(format_bpm(127.98), "128");
        assert_eq!(format_bpm(128.5), "128.5");
    }
}

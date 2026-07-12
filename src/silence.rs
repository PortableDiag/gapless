//! Finds where the music actually starts and stops inside a file.
//!
//! This exists because "gapless" and "no gap" are not the same thing. A pipeline
//! can hand one track to the next with sample-perfect continuity and you will
//! still hear a hole, because plenty of rips have a second or more of digital
//! silence recorded into the file itself. (Measured on a real library: median
//! 1158 ms of trailing silence, worst case 7.4 s.) That silence is real audio
//! data. The only way to not play it is to know where it is.
//!
//! Decodes to 8 kHz mono, which is ample for finding an amplitude edge and about
//! 20x cheaper than decoding at full rate.

use anyhow::{anyhow, Result};
use gst::prelude::*;
use gst_app::AppSink;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Trim {
    /// First sample of actual audio.
    pub start: u64,
    /// One past the last sample of actual audio.
    pub end: u64,
    /// Full decoded length, silence included.
    pub total: u64,
    /// Silent runs *inside* the music, as absolute times in the file. Only runs
    /// long enough to be worth caring about are listed — a musical rest is not a
    /// defect, so short ones are never reported.
    pub holes: Vec<(u64, u64)>,
}

impl Trim {
    /// Audible span, edges excluded.
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    /// The stretches to physically cut, given a cap on how much silence is
    /// allowed to remain inside a track. `limit` of None leaves the track alone.
    ///
    /// This caps rather than removes: a track that pauses for four bars still
    /// pauses. It is the five-minutes-of-nothing-before-a-hidden-track case this
    /// is for, not the rests.
    pub fn cuts(&self, limit: Option<u64>) -> Vec<(u64, u64)> {
        let Some(limit) = limit else { return Vec::new() };
        self.holes
            .iter()
            .filter(|(a, b)| b.saturating_sub(*a) > limit)
            .map(|(a, b)| (a + limit, *b))
            .collect()
    }
}

/// Silence shorter than this inside a track is a musical rest, not a gap.
const MIN_HOLE_NS: u64 = 400_000_000;

const RATE: u64 = 8000;
/// Silence is judged against the track's OWN peak, not an absolute level. A fixed
/// threshold gets this wrong in both directions: too low and the noise floor a
/// lossy codec decodes into a "silent" run-out reads as music (measured: a 991 ms
/// silence detected as 186 ms, because the run-out sat around -50 dBFS); too high
/// and a quietly-mastered track gets its ending amputated.
///
/// 1% of peak is -40 dB relative. Anything below that is not something you can
/// hear next to the rest of the track.
const RELATIVE: f64 = 0.01;
/// ...but never call something silent if it is above -60 dBFS absolute, which
/// protects a track whose peak is itself tiny.
const FLOOR: f64 = 0.001;
/// Keep a little air either side so we never clip an attack or a decay tail.
const GUARD_NS: u64 = 10_000_000;

pub fn analyze(path: &Path) -> Result<Trim> {
    let uri = crate::player::uri_for(path);

    let source = gst::ElementFactory::make("uridecodebin")
        .property("uri", &uri)
        .build()?;
    let convert = gst::ElementFactory::make("audioconvert").build()?;
    let resample = gst::ElementFactory::make("audioresample").build()?;
    let sink = gst::ElementFactory::make("appsink").build()?;

    let pipeline = gst::Pipeline::new();
    pipeline.add_many([&source, &convert, &resample, &sink])?;
    gst::Element::link_many([&convert, &resample, &sink])?;

    let appsink = sink
        .clone()
        .dynamic_cast::<AppSink>()
        .map_err(|_| anyhow!("appsink cast failed"))?;
    appsink.set_caps(Some(
        &gst::Caps::builder("audio/x-raw")
            .field("format", "S16LE")
            .field("channels", 1i32)
            .field("rate", RATE as i32)
            .field("layout", "interleaved")
            .build(),
    ));
    appsink.set_sync(false); // decode as fast as the CPU allows

    // uridecodebin's output pad only appears once it knows what it's decoding.
    let convert_weak = convert.downgrade();
    source.connect_pad_added(move |_, pad| {
        if let Some(convert) = convert_weak.upgrade() {
            if let Some(sinkpad) = convert.static_pad("sink") {
                if !sinkpad.is_linked() {
                    let _ = pad.link(&sinkpad);
                }
            }
        }
    });

    pipeline.set_state(gst::State::Playing)?;

    // Two passes over an 8 kHz mono decode: a few MB even for a long track, and
    // we need the peak before we can say what counts as silence.
    let mut envelope: Vec<f64> = Vec::new();
    while let Ok(sample) = appsink.pull_sample() {
        let Some(buffer) = sample.buffer() else { continue };
        let Ok(map) = buffer.map_readable() else { continue };
        for chunk in map.chunks_exact(2) {
            let v = i16::from_le_bytes([chunk[0], chunk[1]]);
            envelope.push((v as f64 / 32768.0).abs());
        }
    }

    let _ = pipeline.set_state(gst::State::Null);

    let to_ns = |s: usize| (s as u64) * 1_000_000_000 / RATE;
    let total = to_ns(envelope.len());

    if envelope.is_empty() {
        return Ok(Trim::default());
    }

    // 99.9th percentile, not the true max: one stray sample must not set the bar.
    let mut sorted = envelope.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((sorted.len() as f64 * 0.999) as usize).min(sorted.len() - 1);
    let peak = sorted[idx];
    let threshold = (peak * RELATIVE).max(FLOOR);

    let first = envelope.iter().position(|&v| v > threshold);
    let last = envelope.iter().rposition(|&v| v > threshold);

    let (Some(first), Some(last)) = (first, last) else {
        // Entirely silent. Don't trim it to nothing; just play it.
        return Ok(Trim { start: 0, end: total, total, holes: Vec::new() });
    };

    // Silent runs strictly between the first and last audible sample.
    let mut holes = Vec::new();
    let mut run_start: Option<usize> = None;
    for i in first..=last {
        if envelope[i] > threshold {
            if let Some(begin) = run_start.take() {
                let (a, b) = (to_ns(begin), to_ns(i));
                // Leave the guard either side, exactly as at the track edges, so
                // a cut never clips the decay going in or the attack coming out.
                if b.saturating_sub(a) > MIN_HOLE_NS + 2 * GUARD_NS {
                    holes.push((a + GUARD_NS, b - GUARD_NS));
                }
            }
        } else if run_start.is_none() {
            run_start = Some(i);
        }
    }

    Ok(Trim {
        start: to_ns(first).saturating_sub(GUARD_NS),
        end: (to_ns(last) + GUARD_NS).min(total),
        total,
        holes,
    })
}

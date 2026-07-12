//! Playback engine: an `audiomixer` timeline.
//!
//! The first engine used `playbin` + `about-to-finish`, the standard way to get
//! gapless, and it worked — verified sample-exact. It had two hard limits:
//!
//!   1. It plays whatever is in the file. Many rips carry a second or more of
//!      digital silence recorded at the end of every track (measured on a real
//!      library: median 1158 ms, worst 7.4 s). A perfectly gapless pipeline
//!      dutifully plays that silence and you hear a gap. Skipping it needs a
//!      per-track segment, which playbin's gapless handoff cannot express.
//!      (Two other routes were tried and rejected: `removesilence` is mono-only,
//!      and a mid-stream segment-stop seek duplicates ~1 s of buffered audio.)
//!   2. It can only butt tracks together. No crossfade.
//!
//! `audiomixer` solves both with one mechanism. Each track becomes its own
//! branch feeding a mixer pad, and every pad has:
//!
//!   * `offset` — where this track begins on the mixer's timeline. Put it at the
//!     previous track's end and you have gapless; put it earlier and they overlap.
//!   * `volume` — automatable from a control source, which is the crossfade.
//!
//! Crossfade = 0 therefore collapses to exact concatenation, and one code path
//! serves both. Silence is skipped by dropping out-of-range buffers in a pad
//! probe, so the mixer never sees it and the branch hits EOS early.

use anyhow::{anyhow, Result};
use gst::prelude::*;
use gst_controller::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::silence::{self, Trim};

/// Everything downstream of the mixer runs at this format. Fixing it means the
/// sink never renegotiates between tracks — a caps change mid-stream resets the
/// audio sink, which is its own source of gaps.
const RATE: i32 = 44_100;
const CHANNELS: i32 = 2;

#[derive(Debug, Clone)]
pub enum PlayerEvent {
    TrackStarted(usize),
    Position { pos: u64, dur: u64 },
    PlayingChanged(bool),
    /// Repeat or shuffle changed. Unlike the transport events these do not move
    /// the pipeline, so nothing else would tell the UI they happened — and they
    /// can originate from MPRIS as easily as from a button.
    ModesChanged { repeat: Repeat, shuffle: bool },
    QueueFinished,
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repeat {
    Off,
    All,
    One,
}

impl Repeat {
    pub fn cycle(self) -> Self {
        match self {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        }
    }
}

#[derive(Debug, Clone)]
pub struct QueuedTrack {
    pub path: PathBuf,
    /// From the file's tags. Stands in until the silence analysis lands.
    pub duration_nanos: u64,
}

struct Queue {
    tracks: Vec<QueuedTrack>,
    order: Vec<usize>,
    slot_of: Vec<usize>,
    repeat: Repeat,
    shuffle: bool,
}

impl Queue {
    fn after(&self, track: usize) -> Option<usize> {
        let len = self.order.len();
        if len == 0 {
            return None;
        }
        if self.repeat == Repeat::One {
            return Some(track);
        }
        let slot = *self.slot_of.get(track)?;
        match self.repeat {
            Repeat::All => Some(self.order[(slot + 1) % len]),
            Repeat::Off => (slot + 1 < len).then(|| self.order[slot + 1]),
            Repeat::One => unreachable!(),
        }
    }

    fn before(&self, track: usize) -> Option<usize> {
        let len = self.order.len();
        if len == 0 {
            return None;
        }
        let slot = *self.slot_of.get(track)?;
        if slot > 0 {
            Some(self.order[slot - 1])
        } else if self.repeat != Repeat::Off {
            Some(self.order[len - 1])
        } else {
            Some(self.order[0])
        }
    }

    fn reorder(&mut self, keep_first: Option<usize>) {
        let len = self.tracks.len();
        self.order = (0..len).collect();
        if self.shuffle {
            shuffle_in_place(&mut self.order);
            if let Some(cur) = keep_first {
                if let Some(at) = self.order.iter().position(|&t| t == cur) {
                    self.order.swap(0, at);
                }
            }
        }
        self.slot_of = vec![0; len];
        for (slot, &track) in self.order.iter().enumerate() {
            self.slot_of[track] = slot;
        }
    }
}

/// One track, decoding into one mixer pad.
struct Branch {
    slot: u64,
    track: usize,
    bin: gst::Bin,
    pad: gst::Pad,
    /// Where this track begins on the mixer timeline.
    start_rt: u64,
    /// Audible length, silence excluded. None until the analysis lands.
    len: Option<u64>,
    trim: Arc<Mutex<Option<Trim>>>,
    started: Arc<AtomicBool>,
    /// Snapshot of the trim setting when this branch was built. The probe obeys
    /// this, not the live flag: if a toggle changed what the probe drops without
    /// changing the length we already scheduled the next track against, the two
    /// tracks would overlap or leave a hole.
    trim_on: bool,
    /// Whether the track that comes after this one has already been built.
    /// `schedule_following` can be reached from several places; without this it
    /// happily appends the same next track twice.
    followed: bool,
}

#[derive(Default)]
struct Sched {
    branches: Vec<Branch>,
    current: Option<usize>,
    /// The last track we told the UI about. Distinct from `current`, which is set
    /// the moment playback is requested — otherwise the very first track looks
    /// like "no change" to the poller and is never announced at all.
    announced: Option<usize>,
    current_start: u64,
    current_len: u64,
    /// How far into the track the current branch was told to start. The mixer
    /// timeline always begins at zero; the song does not.
    skip: u64,
    next_slot: u64,
    /// Tracks with an analysis in flight, so we never decode the same file twice.
    analyzing: HashSet<usize>,
    finished: bool,
}

enum Internal {
    Analyzed(usize, Trim),
    Eos(u64),
}

pub struct Player {
    pipeline: gst::Pipeline,
    mixer: gst::Element,
    volume: gst::Element,
    queue: Arc<Mutex<Queue>>,
    sched: Arc<Mutex<Sched>>,
    trims: Arc<Mutex<HashMap<PathBuf, Trim>>>,
    trim_enabled: Arc<AtomicBool>,
    /// Crossfade length in nanoseconds. Zero means gapless.
    crossfade: Arc<AtomicU64>,
    /// Cap on silence left *inside* a track, in nanoseconds. Zero = leave alone.
    inner_limit: Arc<AtomicU64>,
    tx: async_channel::Sender<PlayerEvent>,
    itx: async_channel::Sender<Internal>,
    pub events: async_channel::Receiver<PlayerEvent>,
    _bus_watch: gst::bus::BusWatchGuard,
}

impl Player {
    pub fn new() -> Result<Arc<Self>> {
        Self::with_sink(None)
    }

    pub fn with_sink(sink: Option<gst::Element>) -> Result<Arc<Self>> {
        gst::init()?;

        let pipeline = gst::Pipeline::new();
        let mixer = gst::ElementFactory::make("audiomixer")
            .build()
            .map_err(|_| anyhow!("no audiomixer — install gstreamer1.0-plugins-base"))?;
        let convert = gst::ElementFactory::make("audioconvert").build()?;
        let volume = gst::ElementFactory::make("volume").build()?;
        let sink = match sink {
            Some(s) => s,
            None => gst::ElementFactory::make("autoaudiosink").build()?,
        };

        pipeline.add_many([&mixer, &convert, &volume, &sink])?;

        // ReplayGain, when the plugins are present. After the mixer, so it sees
        // the tag events the branches forward.
        let rg: Vec<gst::Element> = ["rgvolume", "rglimiter"]
            .iter()
            .filter_map(|n| gst::ElementFactory::make(n).build().ok())
            .collect();
        for e in &rg {
            pipeline.add(e)?;
        }

        let mut chain: Vec<&gst::Element> = vec![&mixer, &convert];
        chain.extend(rg.iter());
        chain.push(&volume);
        chain.push(&sink);
        gst::Element::link_many(&chain)?;

        let (tx, events) = async_channel::unbounded();
        let (itx, irx) = async_channel::unbounded();

        let queue = Arc::new(Mutex::new(Queue {
            tracks: Vec::new(),
            order: Vec::new(),
            slot_of: Vec::new(),
            repeat: Repeat::Off,
            shuffle: false,
        }));

        let bus_watch = watch_bus(&pipeline, tx.clone())?;

        let player = Arc::new(Player {
            pipeline,
            mixer,
            volume,
            queue,
            sched: Arc::new(Mutex::new(Sched::default())),
            trims: Arc::new(Mutex::new(HashMap::new())),
            trim_enabled: Arc::new(AtomicBool::new(true)),
            crossfade: Arc::new(AtomicU64::new(0)),
            inner_limit: Arc::new(AtomicU64::new(0)),
            tx,
            itx,
            events,
            _bus_watch: bus_watch,
        });

        player.pump_internal(irx);
        player.poll_position();
        Ok(player)
    }

    // ---- queue -------------------------------------------------------

    pub fn set_tracks(&self, tracks: Vec<QueuedTrack>) {
        let mut q = self.queue.lock().unwrap();
        q.tracks = tracks;
        q.reorder(None);
    }

    pub fn set_repeat(&self, repeat: Repeat) {
        let shuffle = {
            let mut q = self.queue.lock().unwrap();
            q.repeat = repeat;
            q.shuffle
        };
        self.emit_modes(repeat, shuffle);
    }

    pub fn repeat(&self) -> Repeat {
        self.queue.lock().unwrap().repeat
    }

    pub fn set_shuffle(&self, shuffle: bool) {
        let keep = self.sched.lock().unwrap().current;
        let repeat = {
            let mut q = self.queue.lock().unwrap();
            q.shuffle = shuffle;
            q.reorder(keep);
            q.repeat
        };
        self.emit_modes(repeat, shuffle);
    }

    /// Callers are on the GTK main thread and the channel is unbounded, so this
    /// cannot block; a closed channel just means we are shutting down.
    fn emit_modes(&self, repeat: Repeat, shuffle: bool) {
        let _ = self.tx.try_send(PlayerEvent::ModesChanged { repeat, shuffle });
    }

    pub fn shuffle(&self) -> bool {
        self.queue.lock().unwrap().shuffle
    }

    pub fn current(&self) -> Option<usize> {
        self.sched.lock().unwrap().current
    }

    // ---- trimming & crossfade ----------------------------------------

    pub fn set_trim_silence(&self, on: bool) {
        if self.trim_enabled.swap(on, Ordering::SeqCst) == on {
            return;
        }
        if self.is_loaded() {
            self.reschedule_ahead();
        }
    }

    pub fn trim_silence(&self) -> bool {
        self.trim_enabled.load(Ordering::SeqCst)
    }

    /// 0 = gapless: tracks butt up exactly. Anything else overlaps them by that
    /// much and crossfades.
    pub fn set_crossfade(&self, nanos: u64) {
        if self.crossfade.swap(nanos, Ordering::SeqCst) == nanos {
            return;
        }
        if self.is_loaded() {
            self.reschedule_ahead();
        }
    }

    pub fn crossfade(&self) -> u64 {
        self.crossfade.load(Ordering::SeqCst)
    }

    /// Cap the silence *inside* a track. 0 leaves tracks untouched.
    ///
    /// Unlike edge trimming, this cannot work by simply dropping buffers: the
    /// mixer would emit silence for the stretch where nothing arrived, and the
    /// hole would still be there. The rest of the track has to be pulled earlier,
    /// which means rewriting its timestamps as it goes past — see `install_probe`.
    pub fn set_inner_limit(&self, nanos: u64) {
        if self.inner_limit.swap(nanos, Ordering::SeqCst) == nanos {
            return;
        }
        if self.is_loaded() {
            self.reschedule_ahead();
        }
    }

    pub fn inner_limit(&self) -> u64 {
        self.inner_limit.load(Ordering::SeqCst)
    }

    fn inner_limit_opt(&self) -> Option<u64> {
        match self.inner_limit.load(Ordering::SeqCst) {
            0 => None,
            n => Some(n),
        }
    }

    // ---- transport ---------------------------------------------------

    pub fn play_index(&self, track: usize) -> Result<()> {
        self.start_at(track, 0)
    }

    /// Start a track part-way in. Resuming last session's position is the same
    /// operation as seeking, so it goes through the same code path.
    pub fn play_index_at(&self, track: usize, offset: u64) -> Result<()> {
        self.start_at(track, offset)
    }

    fn start_at(&self, track: usize, offset: u64) -> Result<()> {
        {
            let q = self.queue.lock().unwrap();
            if track >= q.tracks.len() {
                return Err(anyhow!("index out of range"));
            }
        }

        self.pipeline.set_state(gst::State::Null)?;
        self.teardown_branches();

        {
            let mut s = self.sched.lock().unwrap();
            s.current = Some(track);
            s.announced = None;
            s.current_start = 0;
            s.current_len = 0;
            s.skip = 0;
            s.finished = false;
        }

        // The trim may not be known yet. The probe reads it from a shared cell on
        // every buffer, so an analysis that lands after playback has begun still
        // takes effect on the track's tail — the part that actually matters.
        // `offset` is handed to the branch as its skip: the probe drops everything
        // before it and the pad offset shifts what remains back to running time 0.
        // That IS the seek. Do NOT also fire a pipeline seek — the pad offset is
        // already shifted, so the second seek lands `offset` further on again, off
        // the end of the track, and the branch EOSes with nothing played.
        let trim = self.cached_trim(track);
        self.add_branch(track, 0, trim, offset)?;
        self.request_analysis(track);
        // If the trims are already cached, build the follow-on track NOW, before
        // a single buffer flows. Waiting for the first-buffer callback is a race
        // the pipeline can win: with no next pad, the mixer EOSes the moment this
        // branch ends, and the queue stops dead.
        self.schedule_following();

        // Where we actually are in the track, for the position display: the branch
        // renders from running time 0, but that instant is `offset` into the song.
        self.sched.lock().unwrap().skip = offset;

        self.pipeline.set_state(gst::State::Playing)?;
        Ok(())
    }

    pub fn set_playing(&self, playing: bool) -> Result<()> {
        self.pipeline
            .set_state(if playing { gst::State::Playing } else { gst::State::Paused })?;
        Ok(())
    }

    pub fn toggle_pause(&self) -> Result<bool> {
        let playing = self.is_playing();
        self.set_playing(!playing)?;
        Ok(!playing)
    }

    pub fn is_playing(&self) -> bool {
        self.pipeline.current_state() == gst::State::Playing
    }

    pub fn is_loaded(&self) -> bool {
        matches!(
            self.pipeline.current_state(),
            gst::State::Playing | gst::State::Paused
        )
    }

    pub fn stop(&self) -> Result<()> {
        self.pipeline.set_state(gst::State::Null)?;
        self.teardown_branches();
        let mut s = self.sched.lock().unwrap();
        s.current = None;
        s.announced = None;
        s.current_start = 0;
        s.current_len = 0;
        Ok(())
    }

    /// An explicit skip ignores Repeat::One — you pressed the button because you
    /// want a different song. Only the automatic advance honours One.
    pub fn next(&self) -> Result<()> {
        let current = self.current();
        let target = {
            let q = self.queue.lock().unwrap();
            match current {
                Some(c) => {
                    let slot = q.slot_of.get(c).copied().unwrap_or(0);
                    let len = q.order.len();
                    if slot + 1 < len {
                        Some(q.order[slot + 1])
                    } else if q.repeat != Repeat::Off {
                        q.order.first().copied()
                    } else {
                        None
                    }
                }
                None => q.order.first().copied(),
            }
        };
        match target {
            Some(t) => self.play_index(t),
            None => self.stop(),
        }
    }

    pub fn previous(&self) -> Result<()> {
        let current = self.current();
        let target = {
            let q = self.queue.lock().unwrap();
            match current {
                Some(c) => q.before(c),
                None => q.order.first().copied(),
            }
        };
        match target {
            Some(t) => self.play_index(t),
            None => Ok(()),
        }
    }

    /// Seeking is an explicit jump, so it is allowed to be disruptive: we rebuild
    /// the timeline with the current track at its head. That keeps the mixer's
    /// pad offsets trivially correct, which they would not be if we seeked a
    /// timeline that already had a crossfade scheduled into it.
    pub fn seek(&self, nanos: u64) {
        let Some(track) = self.current() else { return };
        if let Err(e) = self.start_at(track, nanos) {
            eprintln!("seek failed: {e}");
        }
    }

    pub fn set_volume(&self, v: f64) {
        self.volume.set_property("volume", v.clamp(0.0, 1.0));
    }

    pub fn volume(&self) -> f64 {
        self.volume.property::<f64>("volume")
    }

    /// Position within the current track.
    pub fn position(&self) -> u64 {
        let global = self
            .pipeline
            .query_position::<gst::ClockTime>()
            .map(|t| t.nseconds())
            .unwrap_or(0);
        let s = self.sched.lock().unwrap();
        global.saturating_sub(s.current_start) + s.skip
    }

    // ---- branches ----------------------------------------------------

    fn cached_trim(&self, track: usize) -> Option<Trim> {
        let path = {
            let q = self.queue.lock().unwrap();
            q.tracks.get(track)?.path.clone()
        };
        self.trims.lock().unwrap().get(&path).cloned()
    }

    /// What this branch will really be worth on the timeline. With trimming on
    /// that's the audible span; with it off it's the whole decoded file. Getting
    /// this wrong is what makes the next track overlap or leave a gap.
    fn effective_len(trim: &Trim, trim_on: bool, inner: Option<u64>) -> u64 {
        let base = if trim_on { trim.len() } else { trim.total };
        let cut: u64 = trim.cuts(inner).iter().map(|(a, b)| b.saturating_sub(*a)).sum();
        base.saturating_sub(cut)
    }

    /// Decodes every queued track up front to fill the trim cache. Blocking, so
    /// it is for harnesses and short queues — normal playback analyses lazily and
    /// has whole minutes of music in which to do it.
    pub fn preanalyze_blocking(&self) {
        let paths: Vec<PathBuf> = {
            let q = self.queue.lock().unwrap();
            q.tracks.iter().map(|t| t.path.clone()).collect()
        };
        for path in paths {
            if self.trims.lock().unwrap().contains_key(&path) {
                continue;
            }
            if let Ok(trim) = silence::analyze(&path) {
                self.trims.lock().unwrap().insert(path, trim);
            }
        }
    }

    fn track_duration(&self, track: usize) -> u64 {
        self.queue
            .lock()
            .unwrap()
            .tracks
            .get(track)
            .map(|t| t.duration_nanos)
            .unwrap_or(0)
    }

    /// Decode the track at 8 kHz on a worker thread to find where the music
    /// really starts and stops. Never blocks the UI.
    fn request_analysis(&self, track: usize) {
        let path = {
            let q = self.queue.lock().unwrap();
            match q.tracks.get(track) {
                Some(t) => t.path.clone(),
                None => return,
            }
        };
        if self.trims.lock().unwrap().contains_key(&path) {
            return;
        }
        if !self.sched.lock().unwrap().analyzing.insert(track) {
            return;
        }

        let itx = self.itx.clone();
        std::thread::spawn(move || match silence::analyze(&path) {
            Ok(trim) => {
                let _ = itx.send_blocking(Internal::Analyzed(track, trim));
            }
            Err(e) => eprintln!("silence analysis failed for {}: {e}", path.display()),
        });
    }

    fn add_branch(&self, track: usize, start_rt: u64, trim: Option<Trim>, skip: u64) -> Result<()> {
        let path = {
            let q = self.queue.lock().unwrap();
            q.tracks.get(track).ok_or_else(|| anyhow!("bad index"))?.path.clone()
        };

        let (bin, inner_srcpad) = build_branch(&path)?;
        self.pipeline.add(&bin)?;

        let pad = self
            .mixer
            .request_pad_simple("sink_%u")
            .ok_or_else(|| anyhow!("audiomixer refused a pad"))?;
        let srcpad = bin.static_pad("src").ok_or_else(|| anyhow!("branch has no src"))?;
        srcpad.link(&pad)?;


        // The branch's buffers are stamped from the file's start, so shift by the
        // trim point: a buffer at pts == trim.start must emerge at running time
        // start_rt. A negative offset is fine and expected.
        let trim_on = self.trim_silence();
        let inner = self.inner_limit_opt();
        let head = if trim_on {
            trim.as_ref().map(|t| t.start).unwrap_or(0)
        } else {
            0
        } + skip;
        pad.set_offset(start_rt as i64 - head as i64);

        let cuts: Arc<Vec<(u64, u64)>> =
            Arc::new(trim.as_ref().map(|t| t.cuts(inner)).unwrap_or_default());

        let trim_cell = Arc::new(Mutex::new(trim.clone()));
        let started = Arc::new(AtomicBool::new(false));

        let slot = {
            let mut s = self.sched.lock().unwrap();
            s.next_slot += 1;
            s.next_slot
        };

        self.install_probe(
            &inner_srcpad,
            slot,
            trim_cell.clone(),
            started.clone(),
            skip,
            trim_on,
            cuts,
        );

        let len = trim
            .as_ref()
            .map(|t| Self::effective_len(t, trim_on, inner))
            .unwrap_or_else(|| self.track_duration(track));
        self.apply_fade(&pad, start_rt, len);

        self.sched.lock().unwrap().branches.push(Branch {
            slot,
            track,
            bin: bin.clone(),
            pad,
            start_rt,
            len: trim.as_ref().map(|t| Self::effective_len(t, trim_on, inner)),
            trim: trim_cell,
            started,
            trim_on,
            followed: false,
        });

        bin.sync_state_with_parent()?;
        Ok(())
    }

    /// Drops the silence, and reports the first buffer that survives.
    fn install_probe(
        &self,
        srcpad: &gst::Pad,
        slot: u64,
        trim: Arc<Mutex<Option<Trim>>>,
        started: Arc<AtomicBool>,
        skip: u64,
        trim_on: bool,
        cuts: Arc<Vec<(u64, u64)>>,
    ) {
        let itx = self.itx.clone();

        srcpad.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |_, info| match &mut info.data {
                Some(gst::PadProbeData::Buffer(buffer)) => {
                    let pts = buffer.pts().map(|p| p.nseconds()).unwrap_or(0);
                    let dur = buffer.duration().map(|d| d.nseconds()).unwrap_or(0);

                    if trim_on {
                        if let Some(t) = trim.lock().unwrap().clone() {
                            let head = t.start + skip;
                            // Leading silence, plus anything before a seek point.
                            if pts + dur <= head {
                                return gst::PadProbeReturn::Drop;
                            }
                            // Trailing silence. Dropping it makes the branch hit
                            // EOS early, which is what moves the mixer on early —
                            // no seek, no glitch, no duplicated audio.
                            if pts >= t.end {
                                return gst::PadProbeReturn::Drop;
                            }
                        }
                    }

                    if !cuts.is_empty() {
                        // Any buffer that OVERLAPS a cut goes, not merely those
                        // wholly inside it. A buffer straddling the edge would
                        // otherwise be neither dropped (not fully inside) nor
                        // shifted (the cut does not end before it) — it would
                        // sail through at its old timestamp, and that single
                        // backwards jump makes audiomixer resync and discard the
                        // whole rewritten timeline. Dropping the straddlers costs
                        // one buffer of silence at each edge; the cut region is
                        // silence anyway.
                        if cuts.iter().any(|(a, b)| pts < *b && pts + dur > *a) {
                            return gst::PadProbeReturn::Drop;
                        }
                        // Past one or more cuts: pull this buffer earlier by
                        // everything removed before it. Without this the mixer
                        // would simply emit silence over the hole and nothing
                        // would have been gained.
                        let removed: u64 = cuts
                            .iter()
                            .filter(|(_, b)| *b <= pts)
                            .map(|(a, b)| b.saturating_sub(*a))
                            .sum();
                        if removed > 0 {
                            let b = buffer.make_mut();
                            b.set_pts(gst::ClockTime::from_nseconds(pts.saturating_sub(removed)));
                        }
                    }

                    // `started` means "the head of this file has already been let
                    // through", which decides whether a late-arriving analysis can
                    // still trim the head. It does NOT mean the track is audible:
                    // the mixer buffers each branch long before its offset comes
                    // due. What you can actually hear is derived from the playback
                    // position — see `current_at`.
                    started.store(true, Ordering::SeqCst);
                    gst::PadProbeReturn::Ok
                }
                Some(gst::PadProbeData::Event(event)) => {
                    if event.type_() == gst::EventType::Eos {
                        let _ = itx.send_blocking(Internal::Eos(slot));
                    }
                    gst::PadProbeReturn::Ok
                }
                _ => gst::PadProbeReturn::Ok,
            },
        );
    }

    /// The crossfade. At 0 this leaves the pad flat at 1.0 and the tracks simply
    /// abut — gapless. Otherwise it writes an equal-power envelope: fade in over
    /// the overlap at the head, fade out at the tail. Equal-power (sin/cos)
    /// rather than linear, because two linear ramps on uncorrelated material sum
    /// to an audible dip in the middle of the fade.
    /// Idempotent: safe to re-run on a live pad when the crossfade setting
    /// changes, which is why it clears any envelope already bound to the pad
    /// first. Without that, turning crossfade back off would leave the old
    /// automation driving the volume and the track would still fade.
    fn apply_fade(&self, pad: &gst::Pad, start_rt: u64, len: u64) {
        if let Some(existing) = pad.control_binding("volume") {
            pad.remove_control_binding(&existing);
        }

        let xf = self.crossfade.load(Ordering::SeqCst).min(len / 2);
        if xf == 0 || len == 0 {
            pad.set_property("volume", 1.0f64);
            return;
        }

        let cs = gst_controller::InterpolationControlSource::new();
        cs.set_mode(gst_controller::InterpolationMode::Linear);
        let binding = gst_controller::DirectControlBinding::new_absolute(pad, "volume", &cs);
        if pad.add_control_binding(&binding).is_err() {
            pad.set_property("volume", 1.0f64);
            return;
        }
        write_fade(&cs, start_rt, len, xf);
    }

    /// A setting changed mid-playback. Anything already scheduled but not yet
    /// heard was built against the old numbers, so throw it away and rebuild.
    /// The track you are actually listening to is left alone — it keeps the
    /// snapshot it was created with, so its length still matches what the probe
    /// will really drop.
    fn reschedule_ahead(&self) {
        let (doomed, keep): (Vec<u64>, Vec<(gst::Pad, u64, Option<u64>)>) = {
            let s = self.sched.lock().unwrap();
            let any_started = s.branches.iter().any(|b| b.started.load(Ordering::SeqCst));
            let mut doomed = Vec::new();
            let mut keep = Vec::new();
            for (i, b) in s.branches.iter().enumerate() {
                let is_current =
                    b.started.load(Ordering::SeqCst) || (!any_started && i == 0);
                if is_current {
                    keep.push((b.pad.clone(), b.start_rt, b.len));
                } else {
                    doomed.push(b.slot);
                }
            }
            (doomed, keep)
        };

        for slot in doomed {
            self.remove_branch(slot);
        }
        // The kept branch no longer has a successor, so let it grow one again.
        if let Some(b) = self.sched.lock().unwrap().branches.last_mut() {
            b.followed = false;
        }
        for (pad, start_rt, len) in keep {
            if let Some(len) = len {
                self.apply_fade(&pad, start_rt, len);
            }
        }
        self.schedule_following();
    }
}

fn write_fade(
    cs: &gst_controller::InterpolationControlSource,
    start_rt: u64,
    len: u64,
    xf: u64,
) {
    {
        const STEPS: u64 = 24;
        for i in 0..=STEPS {
            let frac = i as f64 / STEPS as f64;
            let t = start_rt + xf * i / STEPS;
            cs.set(
                gst::ClockTime::from_nseconds(t),
                (frac * std::f64::consts::FRAC_PI_2).sin(),
            );
        }
        let fade_out_at = start_rt + len - xf;
        cs.set(gst::ClockTime::from_nseconds(fade_out_at), 1.0);
        for i in 0..=STEPS {
            let frac = i as f64 / STEPS as f64;
            let t = fade_out_at + xf * i / STEPS;
            cs.set(
                gst::ClockTime::from_nseconds(t),
                (frac * std::f64::consts::FRAC_PI_2).cos(),
            );
        }
    }
}

impl Player {
    fn teardown_branches(&self) {
        let branches: Vec<Branch> = std::mem::take(&mut self.sched.lock().unwrap().branches);
        for b in branches {
            let _ = b.bin.set_state(gst::State::Null);
            let _ = self.pipeline.remove(&b.bin);
            self.mixer.release_request_pad(&b.pad);
        }
    }

    fn remove_branch(&self, slot: u64) {
        let branch = {
            let mut s = self.sched.lock().unwrap();
            s.branches
                .iter()
                .position(|b| b.slot == slot)
                .map(|i| s.branches.remove(i))
        };
        if let Some(b) = branch {
            let _ = b.bin.set_state(gst::State::Null);
            let _ = self.pipeline.remove(&b.bin);
            self.mixer.release_request_pad(&b.pad);
        }
    }

    /// Once we know how long the last scheduled track really is, we know when the
    /// one after it should start — and can build it.
    fn schedule_following(&self) {
        let (last_track, last_start, last_len) = {
            let s = self.sched.lock().unwrap();
            match s.branches.last() {
                Some(b) if !b.followed => match b.len {
                    Some(len) => (b.track, b.start_rt, len),
                    None => return, // analysis still pending
                },
                _ => return,
            }
        };

        let Some(next) = self.queue.lock().unwrap().after(last_track) else {
            return; // end of queue with repeat off
        };

        // The next track's trim has to be known before we build it: its leading
        // silence must be dropped from the very first buffer.
        let Some(trim) = self.cached_trim(next) else {
            self.request_analysis(next);
            return;
        };

        let xf = self.crossfade.load(Ordering::SeqCst).min(last_len / 2);
        let start_rt = last_start + last_len.saturating_sub(xf);

        if let Some(b) = self.sched.lock().unwrap().branches.last_mut() {
            b.followed = true;
        }
        if let Err(e) = self.add_branch(next, start_rt, Some(trim), 0) {
            eprintln!("could not schedule next track: {e}");
        }
    }

    /// Which track is audible right now, from the playback position against the
    /// mixer timeline. This is exact — we chose every branch's start time — and,
    /// unlike a first-buffer callback, it is not fooled by the mixer buffering a
    /// track's data long before that track is due.
    fn current_at(&self, pos: u64) -> Option<(usize, u64, u64)> {
        let s = self.sched.lock().unwrap();
        s.branches
            .iter()
            .filter(|b| b.start_rt <= pos)
            .max_by_key(|b| b.start_rt)
            .map(|b| (b.track, b.start_rt, b.len.unwrap_or(0)))
    }

    fn pump_internal(self: &Arc<Self>, irx: async_channel::Receiver<Internal>) {
        let weak = Arc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(msg) = irx.recv().await {
                let Some(player) = weak.upgrade() else { break };
                match msg {
                    Internal::Analyzed(track, trim) => {
                        if let Some(path) = {
                            let q = player.queue.lock().unwrap();
                            q.tracks.get(track).map(|t| t.path.clone())
                        } {
                            player.trims.lock().unwrap().insert(path, trim.clone());
                        }

                        let inner = player.inner_limit_opt();
                        {
                            let mut s = player.sched.lock().unwrap();
                            s.analyzing.remove(&track);
                            let current = s.current;
                            let mut current_len = None;

                            for b in s.branches.iter_mut() {
                                if b.track != track || b.len.is_some() {
                                    continue;
                                }
                                // If the head already played we cannot retro-trim
                                // it, so only the tail trim applies.
                                let effective = if b.started.load(Ordering::SeqCst) {
                                    Trim { start: 0, ..trim.clone() }
                                } else {
                                    trim.clone()
                                };
                                *b.trim.lock().unwrap() = Some(effective.clone());
                                b.len = Some(Player::effective_len(&effective, b.trim_on, inner));
                                if Some(b.track) == current {
                                    current_len = Some(Player::effective_len(&effective, b.trim_on, inner));
                                }
                            }
                            if let Some(len) = current_len {
                                s.current_len = len;
                            }
                        }
                        player.schedule_following();
                    }

                    Internal::Eos(slot) => {
                        player.remove_branch(slot);
                        let done = {
                            let mut s = player.sched.lock().unwrap();
                            if s.branches.is_empty() && !s.finished {
                                s.finished = true;
                                true
                            } else {
                                false
                            }
                        };
                        if done {
                            let _ = player.tx.send_blocking(PlayerEvent::QueueFinished);
                        }
                    }
                }
            }
        });
    }

    fn poll_position(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
            let Some(player) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };

            let global = player
                .pipeline
                .query_position::<gst::ClockTime>()
                .map(|t| t.nseconds())
                .unwrap_or(0);

            if let Some((track, start_rt, len)) = player.current_at(global) {
                let changed = {
                    let mut s = player.sched.lock().unwrap();
                    let changed = s.announced != Some(track);
                    s.current = Some(track);
                    s.announced = Some(track);
                    s.current_start = start_rt;
                    s.current_len = if len > 0 { len } else { 0 };
                    changed
                };
                if player.sched.lock().unwrap().current_len == 0 {
                    let d = player.track_duration(track);
                    player.sched.lock().unwrap().current_len = d;
                }
                if changed {
                    let _ = player.tx.send_blocking(PlayerEvent::TrackStarted(track));
                    player.request_analysis(track);
                    player.schedule_following();
                }
            }

            let dur = player.sched.lock().unwrap().current_len;
            if dur > 0 {
                let _ = player.tx.send_blocking(PlayerEvent::Position {
                    pos: player.position().min(dur),
                    dur,
                });
            }
            glib::ControlFlow::Continue
        });
    }
}

fn build_branch(path: &Path) -> Result<(gst::Bin, gst::Pad)> {
    let bin = gst::Bin::builder().build();

    let source = gst::ElementFactory::make("uridecodebin")
        .property("uri", uri_for(path))
        .build()?;
    let convert = gst::ElementFactory::make("audioconvert").build()?;
    let resample = gst::ElementFactory::make("audioresample").build()?;
    let caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("rate", RATE)
                .field("channels", CHANNELS)
                .field("layout", "interleaved")
                .build(),
        )
        .build()?;
    let queue = gst::ElementFactory::make("queue")
        .property("max-size-time", 2_000_000_000u64)
        .build()?;

    bin.add_many([&source, &convert, &resample, &caps, &queue])?;
    gst::Element::link_many([&convert, &resample, &caps, &queue])?;

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

    let srcpad = queue.static_pad("src").ok_or_else(|| anyhow!("queue has no src"))?;
    let ghost = gst::GhostPad::with_target(&srcpad)?;
    bin.add_pad(&ghost)?;

    // The real pad, not the ghost. A probe on the ghost can drop a buffer, but a
    // buffer *modified* there does not survive the proxy hop to the mixer — the
    // rewritten timestamps were silently discarded. Probe the queue's own src pad
    // instead, where the edit sticks.
    Ok((bin, srcpad.upcast()))
}

fn watch_bus(
    pipeline: &gst::Pipeline,
    tx: async_channel::Sender<PlayerEvent>,
) -> Result<gst::bus::BusWatchGuard> {
    let bus = pipeline.bus().ok_or_else(|| anyhow!("pipeline has no bus"))?;
    let weak = pipeline.downgrade();

    let guard = bus.add_watch_local(move |_, msg| {
        use gst::MessageView;
        match msg.view() {
            MessageView::StateChanged(s) => {
                if let Some(pipeline) = weak.upgrade() {
                    if s.src() == Some(pipeline.upcast_ref()) {
                        let _ = tx.send_blocking(PlayerEvent::PlayingChanged(
                            s.current() == gst::State::Playing,
                        ));
                    }
                }
            }
            MessageView::Error(e) => {
                let _ = tx.send_blocking(PlayerEvent::Error(format!(
                    "{}: {}",
                    e.error(),
                    e.debug().unwrap_or_default()
                )));
            }
            _ => {}
        }
        glib::ControlFlow::Continue
    })?;

    Ok(guard)
}

fn shuffle_in_place(v: &mut [usize]) {
    let mut state = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1;
    for i in (1..v.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        v.swap(i, (state % (i as u64 + 1)) as usize);
    }
}

/// `glib::filename_to_uri` requires an absolute path and errors on a relative
/// one. The naive fallback — `format!("file://{path}")` — is worse than useless
/// there: in `file://testdata/song.mp3` the leading segment parses as a
/// *hostname*, so GStreamer looks for /song.mp3 at the filesystem root.
pub fn uri_for(path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    glib::filename_to_uri(&absolute, None)
        .map(|s| s.to_string())
        .unwrap_or_else(|_| format!("file://{}", absolute.display()))
}

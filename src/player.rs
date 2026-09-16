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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::silence::{self, Trim};
use crate::tempo;

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
    ModesChanged { repeat: Repeat, shuffle: Shuffle },
    /// A tempo measurement finished. `bpm: None` means the track was analysed
    /// and has no steady tempo. The front-end persists this — the engine's copy
    /// dies with the process, and re-measuring a library every session would be
    /// a decode per track for an answer that cannot change.
    TempoMeasured { track: usize, bpm: Option<f64> },
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

/// Three states, not two. `Favorites` is still a shuffle — every track plays
/// exactly once per pass — but the *order* is drawn with the higher-rated tracks
/// weighted towards the front. It is not a filter: a 1-star track can still come
/// up, just rarely, which is the difference between "prefer favorites" and "play
/// only favorites".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shuffle {
    Off,
    On,
    Favorites,
}

impl Shuffle {
    pub fn cycle(self) -> Self {
        match self {
            Shuffle::Off => Shuffle::On,
            Shuffle::On => Shuffle::Favorites,
            Shuffle::Favorites => Shuffle::Off,
        }
    }

    pub fn is_on(self) -> bool {
        self != Shuffle::Off
    }

    /// For `state.json` and anywhere else this has to survive as text.
    pub fn as_str(self) -> &'static str {
        match self {
            Shuffle::Off => "off",
            Shuffle::On => "on",
            Shuffle::Favorites => "favorites",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "on" => Shuffle::On,
            "favorites" => Shuffle::Favorites,
            _ => Shuffle::Off,
        }
    }
}

/// How much more likely a rating makes a track to land early in a favorites
/// shuffle. Doubling per star, with unrated sitting at 2 — the same as a 2-star
/// track, because "I have not judged this" is not the same statement as "I do
/// not like this" and should not be punished like one.
///
/// The ramp is steep on purpose. With linear weights (5 stars = 5, unrated = 1)
/// a real library — which is overwhelmingly unrated — buries its handful of
/// 5-star tracks under sheer volume, and the mode does nothing you can hear.
/// Doubling makes a 5-star track 8× as likely as an unrated one to come up next
/// and 16× a 1-star, which is visible in a single pass.
pub fn weight_for(stars: u8) -> f64 {
    match stars {
        1 => 1.0,
        2 => 2.0,
        3 => 4.0,
        4 => 8.0,
        5 => 16.0,
        _ => 2.0,
    }
}

#[derive(Debug, Clone)]
pub struct QueuedTrack {
    pub path: PathBuf,
    /// From the file's tags. Stands in until the silence analysis lands.
    pub duration_nanos: u64,
    /// 1–5, or 0 for unrated. Read from the ratings sidecar, not from the file —
    /// see `src/ratings.rs`. The engine only cares about it for `Shuffle::Favorites`.
    pub rating: u8,
}

struct Queue {
    tracks: Vec<QueuedTrack>,
    order: Vec<usize>,
    slot_of: Vec<usize>,
    repeat: Repeat,
    shuffle: Shuffle,
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
        if self.shuffle.is_on() {
            let mut rng = Rng::from_clock();
            match self.shuffle {
                Shuffle::Favorites => {
                    let weights: Vec<f64> =
                        self.tracks.iter().map(|t| weight_for(t.rating)).collect();
                    weighted_shuffle_in_place(&mut self.order, &weights, &mut rng);
                }
                _ => shuffle_in_place(&mut self.order, &mut rng),
            }
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
    /// Audible length of the *whole* track, silence excluded. None until the
    /// analysis lands. This is the song's length — what the seek bar shows — and
    /// is **not** what this branch occupies on the timeline when it was resumed
    /// part-way in. For that, see `span`.
    len: Option<u64>,
    /// How far into the track this branch was told to start: 0 normally, the
    /// resume or seek offset otherwise.
    skip: u64,
    trim: Arc<Mutex<Option<Trim>>>,
    started: Arc<AtomicBool>,
    /// Snapshot of the trim setting when this branch was built. The probe obeys
    /// this, not the live flag: if a toggle changed what the probe drops without
    /// changing the length we already scheduled the next track against, the two
    /// tracks would overlap or leave a hole.
    trim_on: bool,
    /// Force Tempo's playback speed for this branch, snapshotted at build time
    /// for exactly the same reason as `trim_on`: it is what the pad offset, the
    /// fade envelope and the follower's start time were all computed against.
    /// Changing it under a live branch would move all three and desynchronise
    /// the timeline, so a settings change instead rebuilds the branches that
    /// have not started — see `reschedule_ahead`.
    speed: f64,
    /// Whether the track that comes after this one has already been built.
    /// `schedule_following` can be reached from several places; without this it
    /// happily appends the same next track twice.
    followed: bool,
}

impl Branch {
    /// How much time this branch really occupies on the mixer timeline. A branch
    /// resumed 227 s into a 300 s song only plays the 73 s that are left, even
    /// though the *song* is still 300 s long — and it is this number, not the
    /// song's length, that says when the next track must start and where the
    /// fade-out belongs. Confusing the two schedules the follower minutes into
    /// the future: dead air, no advance, no crossfade.
    ///
    /// **This is wall-clock time, and `len` and `skip` are not.** The mixer
    /// timeline is measured in seconds of listening; a track is measured in
    /// seconds of track. At 1.3x those are different quantities, and this is the
    /// boundary between them — a fade or a follower timed against the unscaled
    /// number starts late and gets cut off by the transition.
    fn span(&self) -> Option<u64> {
        self.len
            .map(|len| tempo::wall_clock(len.saturating_sub(self.skip), self.speed))
    }
}

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
    /// The current branch's speed, so a position on the mixer timeline can be
    /// turned back into a position in the song. Without it the seek bar runs at
    /// the wrong rate the moment Force Tempo is on.
    speed: f64,
    next_slot: u64,
    /// Tracks with an analysis in flight, so we never decode the same file twice.
    analyzing: HashSet<usize>,
    finished: bool,
}

/// Hand-written rather than derived because `speed` must start at 1.0. A derived
/// `Default` gives it 0.0, and a zero speed turns every media/wall-clock
/// conversion into a division by the `MIN_SPEED` floor — the position display
/// would read twenty times too high before a single track had been loaded.
impl Default for Sched {
    fn default() -> Self {
        Self {
            branches: Vec::new(),
            current: None,
            announced: None,
            current_start: 0,
            current_len: 0,
            skip: 0,
            speed: tempo::UNCHANGED,
            next_slot: 0,
            analyzing: HashSet::new(),
            finished: false,
        }
    }
}

/// Why a branch is going away.
///
/// It decides whether the branch's audio may be thrown away, and getting it
/// wrong fails in opposite directions: flushing a branch that finished on its
/// own puts a click in every track change, and *not* flushing one that is being
/// replaced can deadlock the main thread. See `Player::dispose_branch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposal {
    /// It reached EOS. Its audio is still queued in the mixer and must play out.
    Finished,
    /// It is being replaced or abandoned, and its audio is not wanted.
    Discarded,
}

enum Internal {
    Analyzed(usize, Trim),
    /// A tempo measurement landed: the track, and its speed-determining BPM.
    /// `None` means analysed and found to have no steady tempo — a real answer,
    /// and one that must be recorded so the track is never analysed again.
    Tempo(usize, Option<f64>),
    Eos(u64),
}

pub struct Player {
    pipeline: gst::Pipeline,
    mixer: gst::Element,
    volume: gst::Element,
    /// Kept so we can report what audio is actually going *to*.
    ///
    /// `autoaudiosink` is a bin that picks a real sink at run time, and when that
    /// choice goes wrong the pipeline still reaches PLAYING and the position
    /// still advances — it renders to nothing. That happened on this machine
    /// right after an in-place update: the app reported playing, the position
    /// climbed, and no stream was attached to the audio device at all. Nothing
    /// the player exposed could tell the difference between that and working.
    sink: gst::Element,
    queue: Arc<Mutex<Queue>>,
    sched: Arc<Mutex<Sched>>,
    trims: Arc<Mutex<HashMap<PathBuf, Trim>>>,
    trim_enabled: Arc<AtomicBool>,
    /// Crossfade length in nanoseconds. Zero means gapless.
    crossfade: Arc<AtomicU64>,
    /// Cap on silence left *inside* a track, in nanoseconds. Zero = leave alone.
    inner_limit: Arc<AtomicU64>,
    /// Force Tempo: bring every track to one pace. Off by default — it changes
    /// what the music sounds like, which is not something a player should start
    /// doing on its own.
    force_tempo: Arc<AtomicBool>,
    target_bpm: Arc<AtomicU32>,
    /// The stretch ceiling, as a percentage. See `tempo::speed_for`.
    max_change_percent: Arc<AtomicU32>,
    only_faster: Arc<AtomicBool>,
    /// Resolved tempo per track. The outer `Option` is "have we looked?", the
    /// inner one is "was there anything there?" — those are different states and
    /// collapsing them means either re-measuring a podcast forever or inventing
    /// a tempo for it. The authoritative store is `bpm::BpmStore` on disk; this
    /// is the engine's copy of what it needs to schedule with.
    tempos: Arc<Mutex<HashMap<PathBuf, Option<f64>>>>,
    /// Tracks with a tempo measurement in flight, so one file is never decoded
    /// for its tempo twice at once.
    measuring: Arc<Mutex<HashSet<PathBuf>>>,
    /// Where the last session stopped: (track, offset in nanoseconds). Cued but
    /// **not** started — a player that begins blaring on login is a player you
    /// uninstall — and consumed by the first thing that asks to play.
    ///
    /// It lives here rather than in the GTK front-end because it is not a UI
    /// concern: a media key, an MPRIS client and the control API all have to
    /// resume the same point the play button does. It used to live in `Ui`,
    /// which `mpris.rs` cannot see, so a lock-screen Play on a freshly launched
    /// player silently started the queue from track 0 and threw the resume point
    /// away.
    cued: Mutex<Option<(usize, u64)>>,
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

        let sink_handle = sink.clone();
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
            shuffle: Shuffle::Off,
        }));

        let bus_watch = watch_bus(&pipeline, tx.clone())?;

        let player = Arc::new(Player {
            pipeline,
            mixer,
            volume,
            sink: sink_handle,
            queue,
            sched: Arc::new(Mutex::new(Sched::default())),
            trims: Arc::new(Mutex::new(HashMap::new())),
            trim_enabled: Arc::new(AtomicBool::new(true)),
            crossfade: Arc::new(AtomicU64::new(0)),
            inner_limit: Arc::new(AtomicU64::new(0)),
            force_tempo: Arc::new(AtomicBool::new(false)),
            target_bpm: Arc::new(AtomicU32::new(tempo::DEFAULT_TARGET_BPM)),
            max_change_percent: Arc::new(AtomicU32::new(tempo::DEFAULT_MAX_CHANGE_PERCENT)),
            only_faster: Arc::new(AtomicBool::new(tempo::DEFAULT_ONLY_FASTER)),
            tempos: Arc::new(Mutex::new(HashMap::new())),
            measuring: Arc::new(Mutex::new(HashSet::new())),
            cued: Mutex::new(None),
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

    /// `set_tracks` renumbers everything, so a cue held against the old queue is
    /// meaningless. Callers that re-cue do so *after* loading.
    pub fn clear_cue(&self) {
        *self.cued.lock().unwrap() = None;
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

    pub fn set_shuffle(&self, shuffle: Shuffle) {
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
    fn emit_modes(&self, repeat: Repeat, shuffle: Shuffle) {
        let _ = self.tx.try_send(PlayerEvent::ModesChanged { repeat, shuffle });
    }

    pub fn shuffle(&self) -> Shuffle {
        self.queue.lock().unwrap().shuffle
    }

    /// A rating changed. Stored on the queued track so the *next* favorites
    /// reshuffle sees it; the order in flight is deliberately left alone —
    /// resequencing the queue under the user because they clicked a star is not
    /// what clicking a star means.
    pub fn set_rating(&self, track: usize, stars: u8) {
        let mut q = self.queue.lock().unwrap();
        if let Some(t) = q.tracks.get_mut(track) {
            t.rating = stars;
        }
    }

    pub fn current(&self) -> Option<usize> {
        self.sched.lock().unwrap().current
    }

    // ---- the resume point --------------------------------------------

    /// Cue a track without starting it.
    pub fn set_cued(&self, point: Option<(usize, u64)>) {
        *self.cued.lock().unwrap() = point;
    }

    /// Look without consuming — for saving the session while still cued.
    pub fn cued(&self) -> Option<(usize, u64)> {
        *self.cued.lock().unwrap()
    }

    /// Start playing: the cued resume point if there is one, otherwise carry on
    /// from wherever the pipeline is, otherwise the top of the queue.
    ///
    /// This is the whole of "press play" and every caller goes through it — the
    /// button, a media key, MPRIS, the control API. Anything that reimplements
    /// it is how the resume point gets lost by one route and not another.
    pub fn play(&self) -> Result<()> {
        // Take the cue into a local FIRST. Written as
        // `if let Some(x) = self.cued.lock().unwrap().take()`, the temporary
        // `MutexGuard` lives to the end of the `if let` body in edition 2021 —
        // and the body calls `start_at`, which locks the same mutex to clear the
        // cue. That is a deadlock: the app stays alive, MPRIS answers, and every
        // call then times out with the main loop wedged. Found by the harness,
        // which is the only reason it is not in a release.
        let cue = self.cued.lock().unwrap().take();
        if let Some((track, offset)) = cue {
            return self.play_index_at(track, offset);
        }
        if self.is_loaded() {
            return self.set_playing(true);
        }
        let first = self.queue.lock().unwrap().order.first().copied();
        match first {
            Some(t) => self.play_index(t),
            None => Err(anyhow!("nothing loaded")),
        }
    }

    /// Play/pause, honouring the resume point when nothing is loaded yet.
    pub fn play_pause(&self) -> Result<()> {
        if self.is_loaded() {
            self.toggle_pause()?;
            return Ok(());
        }
        self.play()
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

    // ---- force tempo -------------------------------------------------

    /// Bring every track to one pace.
    ///
    /// Like the other playback settings this takes effect on the tracks that
    /// have not started yet, not on the one you are listening to: its speed is
    /// baked into a pad offset, a fade envelope and the follower's start time,
    /// and moving it under a live branch would desynchronise all three. So the
    /// only track ever heard at the wrong speed is the one that was already
    /// playing when you switched it on.
    pub fn set_force_tempo(&self, on: bool) {
        if self.force_tempo.swap(on, Ordering::SeqCst) == on {
            return;
        }
        if on {
            // The track already playing keeps its speed, but its tempo is still
            // worth knowing: it is what the now-playing panel shows and lets you
            // edit, and caching it now means this track is not unmeasured again
            // next time it comes round.
            if let Some(track) = self.current() {
                self.request_tempo_if_wanted(track);
            }
        }
        if self.is_loaded() {
            self.reschedule_ahead();
        }
    }

    pub fn force_tempo(&self) -> bool {
        self.force_tempo.load(Ordering::SeqCst)
    }

    pub fn set_target_bpm(&self, bpm: u32) {
        let bpm = bpm.clamp(tempo::MIN_TARGET_BPM, tempo::MAX_TARGET_BPM);
        if self.target_bpm.swap(bpm, Ordering::SeqCst) == bpm {
            return;
        }
        if self.force_tempo() && self.is_loaded() {
            self.reschedule_ahead();
        }
    }

    pub fn target_bpm(&self) -> u32 {
        self.target_bpm.load(Ordering::SeqCst)
    }

    pub fn set_max_change_percent(&self, percent: u32) {
        let percent = percent.min(tempo::MAX_STRETCH_PERCENT);
        if self.max_change_percent.swap(percent, Ordering::SeqCst) == percent {
            return;
        }
        if self.force_tempo() && self.is_loaded() {
            self.reschedule_ahead();
        }
    }

    pub fn max_change_percent(&self) -> u32 {
        self.max_change_percent.load(Ordering::SeqCst)
    }

    pub fn set_only_faster(&self, on: bool) {
        if self.only_faster.swap(on, Ordering::SeqCst) == on {
            return;
        }
        if self.force_tempo() && self.is_loaded() {
            self.reschedule_ahead();
        }
    }

    pub fn only_faster(&self) -> bool {
        self.only_faster.load(Ordering::SeqCst)
    }

    /// Tell the engine a track's tempo. `None` records "no steady tempo", which
    /// is why this takes an `Option` rather than treating 0 as absent.
    ///
    /// The front-end owns the persistent store (`bpm::BpmStore`) and pushes
    /// what it knows in here — including a number the user typed over the
    /// measurement, which is why this exists as a public entry point at all.
    pub fn set_track_tempo(&self, path: &Path, bpm: Option<f64>) {
        let changed = {
            let mut t = self.tempos.lock().unwrap();
            t.insert(path.to_path_buf(), bpm) != Some(bpm)
        };
        if changed && self.force_tempo() && self.is_loaded() {
            self.reschedule_ahead();
        }
    }

    pub fn track_tempo(&self, path: &Path) -> Option<Option<f64>> {
        self.tempos.lock().unwrap().get(path).copied()
    }

    /// Forget what we know about a track's tempo, so it is measured again.
    ///
    /// Distinct from `set_track_tempo(path, None)`, which records "there is no
    /// steady tempo here" — a finding that is deliberately never revisited.
    /// This is "I don't know either, have another go".
    pub fn forget_track_tempo(&self, path: &Path) {
        let had = self.tempos.lock().unwrap().remove(path).is_some();
        if had && self.force_tempo() && self.is_loaded() {
            self.reschedule_ahead();
        }
    }

    /// The speed a given track will play at. `1.0` whenever Force Tempo is off,
    /// the tempo is unknown, or the track has none.
    pub fn speed_for_track(&self, track: usize) -> f64 {
        if !self.force_tempo() {
            return tempo::UNCHANGED;
        }
        let Some(path) = self.track_path(track) else {
            return tempo::UNCHANGED;
        };
        self.speed_for_path(&path)
    }

    fn speed_for_path(&self, path: &Path) -> f64 {
        if !self.force_tempo() {
            return tempo::UNCHANGED;
        }
        // Copied out and the guard dropped before anything else runs, rather than
        // matched on in place: a temporary guard in a `match` scrutinee is held
        // for every arm, and an arm that later grows a call back into the player
        // would deadlock. See `schedule_following` for that bug in its live form.
        let known = self.tempos.lock().unwrap().get(path).copied();
        match known {
            Some(Some(bpm)) => {
                tempo::speed_for(bpm, self.target_bpm(), self.max_change_percent(), self.only_faster())
            }
            // Either not measured yet or measured and found to have none. Both
            // play unchanged; the difference is whether we will look again.
            _ => tempo::UNCHANGED,
        }
    }

    fn track_path(&self, track: usize) -> Option<PathBuf> {
        self.queue.lock().unwrap().tracks.get(track).map(|t| t.path.clone())
    }

    /// Measure a track's tempo if Force Tempo is on and we do not know it yet.
    ///
    /// Called for the track that is *starting*, as well as for the one being
    /// scheduled after it. Only doing the latter looks right — the current
    /// track's speed is already fixed and cannot change under it — and is wrong
    /// for two reasons that only show up when you use the feature rather than
    /// test it: the now-playing panel has nowhere to get the BPM it is supposed
    /// to show you and edit, so it says "working out the tempo…" forever; and the
    /// answer is never cached, so the track is still unmeasured the next time it
    /// comes round and plays unstretched again.
    ///
    /// Gated on the feature being on, because it costs a decode per track and
    /// nothing reads the answer otherwise.
    fn request_tempo_if_wanted(&self, track: usize) {
        if self.force_tempo() {
            self.request_tempo(track);
        }
    }

    /// Measure a track's tempo on a worker thread, unless it is already known or
    /// already in flight. A failed decode records "no steady tempo" rather than
    /// nothing: a track whose analysis errors must not be retried forever, and
    /// must never be able to stall the scheduler waiting for an answer that will
    /// not come.
    fn request_tempo(&self, track: usize) {
        let Some(path) = self.track_path(track) else { return };
        if self.tempos.lock().unwrap().contains_key(&path) {
            return;
        }
        if !self.measuring.lock().unwrap().insert(path.clone()) {
            return;
        }

        let itx = self.itx.clone();
        std::thread::spawn(move || {
            // The file's own tag beats a measurement: whoever tagged it had the
            // whole track, and usually the sleeve.
            let found = match crate::bpm::from_tag(&path) {
                Some(bpm) => Some(bpm),
                None => match crate::bpm::detect(&path) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("tempo analysis failed for {}: {e}", path.display());
                        None
                    }
                },
            };
            let _ = itx.send_blocking(Internal::Tempo(track, found));
        });
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
        // Any explicit start supersedes the cue, so it can never be resumed
        // later on top of whatever the user actually chose.
        *self.cued.lock().unwrap() = None;
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
        self.request_tempo_if_wanted(track);
        // If the trims are already cached, build the follow-on track NOW, before
        // a single buffer flows. Waiting for the first-buffer callback is a race
        // the pipeline can win: with no next pad, the mixer EOSes the moment this
        // branch ends, and the queue stops dead.
        self.schedule_following();

        // Where we actually are in the track, for the position display: the branch
        // renders from running time 0, but that instant is `offset` into the song.
        // The speed goes in at the same time and for the same reason — until the
        // position poller runs a quarter of a second from now, `position()` would
        // otherwise convert against whatever the *previous* track was playing at.
        // Computed before the lock is taken, not inside it: `speed_for_track`
        // locks the queue and the tempo map, and everywhere else in this file
        // takes those *before* `sched`. Reversing that here would be a lock-order
        // inversion — the kind that deadlocks once, on someone else's machine.
        let speed = self.speed_for_track(track);
        {
            let mut s = self.sched.lock().unwrap();
            s.skip = offset;
            s.speed = speed;
        }

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

    /// True while the pipeline is playing **or on its way there**.
    ///
    /// A state change is asynchronous, so for a moment after `play()` the
    /// pipeline is still PAUSED with PLAYING pending. Reading only the current
    /// state made `POST /api/play` answer `"playing": false` about a call that
    /// had just succeeded — the caller then has no way to tell "starting" from
    /// "refused" except by polling. The pending state is exactly the missing
    /// information, and it is what the transport asked for.
    pub fn is_playing(&self) -> bool {
        let (_, current, pending) = self.pipeline.state(gst::ClockTime::ZERO);
        current == gst::State::Playing || pending == gst::State::Playing
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

    /// What audio is really going to — the element `autoaudiosink` actually
    /// chose, not the bin's own name.
    ///
    /// Answering "is it playing?" from the pipeline state is not enough. A sink
    /// that failed to open the device leaves the pipeline PLAYING and the
    /// position advancing while nothing reaches the speakers, which is
    /// indistinguishable from working unless something names the sink. Reported
    /// by the control API so a caller — or a person — can see it.
    pub fn audio_sink(&self) -> String {
        // `autoaudiosink` is a bin; the element that matters is the child it
        // picked. An explicitly supplied sink is not a bin and answers for itself.
        let chosen = self
            .sink
            .downcast_ref::<gst::Bin>()
            .and_then(|bin| bin.iterate_sinks().into_iter().flatten().next())
            .map(|e| e.factory().map(|f| f.name().to_string()).unwrap_or_default());

        match chosen {
            Some(name) if !name.is_empty() => name,
            _ => self
                .sink
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_else(|| "unknown".into()),
        }
    }

    /// Position within the current track.
    pub fn position(&self) -> u64 {
        let global = self
            .pipeline
            .query_position::<gst::ClockTime>()
            .map(|t| t.nseconds())
            .unwrap_or(0);
        let s = self.sched.lock().unwrap();
        // The pipeline's position is on the clock; the seek bar is in the song.
        // Those are the same number only at 1.0x.
        tempo::media(global.saturating_sub(s.current_start), s.speed) + s.skip
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

        let speed = self.speed_for_path(&path);
        let (bin, inner_srcpad, exit_pad) = build_branch(&path, speed)?;
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
        //
        // `head` is in media time — the probe that uses it runs upstream of the
        // stretcher and compares against trim points measured in the file. The
        // pad offset is in wall-clock time, because that is what the mixer
        // timeline is, so the conversion happens here and nowhere else. At 1.0x
        // the two are the same number and this reduces to what it always was.
        let trim_on = self.trim_silence();
        let inner = self.inner_limit_opt();
        let head = if trim_on {
            trim.as_ref().map(|t| t.start).unwrap_or(0)
        } else {
            0
        } + skip;
        pad.set_offset(start_rt as i64 - tempo::wall_clock(head, speed) as i64);

        let cuts: Arc<Vec<(u64, u64)>> =
            Arc::new(trim.as_ref().map(|t| t.cuts(inner)).unwrap_or_default());

        let trim_cell = Arc::new(Mutex::new(trim.clone()));
        let started = Arc::new(AtomicBool::new(false));

        let slot = {
            let mut s = self.sched.lock().unwrap();
            s.next_slot += 1;
            s.next_slot
        };

        self.install_probe(&inner_srcpad, trim_cell.clone(), started.clone(), skip, trim_on, cuts);
        self.install_eos_probe(&exit_pad, slot);

        // The fade goes on what this branch will actually play. Resumed part-way
        // in, that is the remainder — not the length of the song. And it is
        // written onto the mixer timeline, so it is counted on the clock: at
        // 1.3x the last six seconds of a track are 4.6 seconds of listening, and
        // a fade timed against the track's own six would start late and be cut
        // off by the transition.
        let full = trim
            .as_ref()
            .map(|t| Self::effective_len(t, trim_on, inner))
            .unwrap_or_else(|| self.track_duration(track));
        self.apply_fade(&pad, start_rt, tempo::wall_clock(full.saturating_sub(skip), speed));

        self.sched.lock().unwrap().branches.push(Branch {
            slot,
            track,
            bin: bin.clone(),
            pad,
            start_rt,
            len: trim.as_ref().map(|t| Self::effective_len(t, trim_on, inner)),
            skip,
            trim: trim_cell,
            started,
            trim_on,
            speed,
            followed: false,
        });

        bin.sync_state_with_parent()?;
        Ok(())
    }

    /// Drops the silence, and reports the first buffer that survives.
    /// Retire the branch when the audio has actually finished leaving it. See
    /// `build_branch` for why this is not the pad the buffers are edited on.
    fn install_eos_probe(&self, exit: &gst::Pad, slot: u64) {
        let itx = self.itx.clone();
        exit.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
            if let Some(gst::PadProbeData::Event(event)) = &info.data {
                if event.type_() == gst::EventType::Eos {
                    let _ = itx.send_blocking(Internal::Eos(slot));
                }
            }
            gst::PadProbeReturn::Ok
        });
    }

    fn install_probe(
        &self,
        srcpad: &gst::Pad,
        trim: Arc<Mutex<Option<Trim>>>,
        started: Arc<AtomicBool>,
        skip: u64,
        trim_on: bool,
        cuts: Arc<Vec<(u64, u64)>>,
    ) {
        srcpad.add_probe(
            gst::PadProbeType::BUFFER,
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
                    keep.push((b.pad.clone(), b.start_rt, b.span()));
                } else {
                    doomed.push(b.slot);
                }
            }
            (doomed, keep)
        };

        for slot in doomed {
            // Discarded, not finished: these branches are being replaced and
            // their audio is not wanted. They may also be blocked pushing into a
            // paused mixer, which is why this has to say so — see `dispose_branch`.
            self.discard_branch(slot);
        }
        // The kept branch no longer has a successor, so let it grow one again.
        if let Some(b) = self.sched.lock().unwrap().branches.last_mut() {
            b.followed = false;
        }
        for (pad, start_rt, span) in keep {
            if let Some(span) = span {
                self.apply_fade(&pad, start_rt, span);
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
            // Everything here is being abandoned — this runs on the way into a
            // new track — so none of it is waiting to be heard, and any of it
            // may be blocked pushing into a mixer that is not consuming.
            self.dispose_branch(b, Disposal::Discarded);
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
            self.dispose_branch(b, Disposal::Finished);
        }
    }

    /// Tear one branch down.
    ///
    /// `Discarded` sends FLUSH_START to the mixer pad first, and that is not an
    /// optimisation — without it the application deadlocks:
    ///
    /// A branch that has filled its queue sits blocked inside `gst_pad_push`,
    /// waiting for the mixer to take a buffer. While the pipeline is PAUSED the
    /// mixer never will. That thread holds the pad's stream lock, and
    /// `set_state(Null)` deactivates the pad, which wants the same lock — so the
    /// caller waits forever. The caller is the GTK main thread: the window stops
    /// repainting and the control API accepts connections it will never answer,
    /// with nothing printed anywhere, because nothing crashed. FLUSH_START is the
    /// one event meant to be sent from another thread for exactly this — it sets
    /// the flushing flag *without* taking the stream lock, so the blocked push
    /// returns `FLUSHING` and the streaming thread unwinds. No FLUSH_STOP is
    /// needed; the pad is released two lines later.
    ///
    /// **This predates Force Tempo.** Changing any playback setting twice while
    /// paused rebuilds the branches twice and hits it; reproduced on v0.5.1 with
    /// two crossfade changes and no tempo code in the process at all.
    ///
    /// `Finished` must NOT flush. A branch that reached EOS on its own still has
    /// audio inside the mixer that has not been played yet, and flushing throws
    /// it away: measured at 8.71 ms of silence and a 3.45 rad phase step at the
    /// splice — a click, on every track change, which is the entire defect this
    /// player exists to fix.
    fn dispose_branch(&self, b: Branch, why: Disposal) {
        if why == Disposal::Discarded {
            b.pad.send_event(gst::event::FlushStart::new());
        }
        let _ = b.bin.set_state(gst::State::Null);
        let _ = self.pipeline.remove(&b.bin);
        self.mixer.release_request_pad(&b.pad);
    }

    /// Discard a branch that is being replaced rather than one that has ended.
    fn discard_branch(&self, slot: u64) {
        let branch = {
            let mut s = self.sched.lock().unwrap();
            s.branches.iter().position(|b| b.slot == slot).map(|i| s.branches.remove(i))
        };
        if let Some(b) = branch {
            self.dispose_branch(b, Disposal::Discarded);
        }
    }

    /// Once we know how long the last scheduled track really is, we know when the
    /// one after it should start — and can build it.
    fn schedule_following(&self) {
        let (last_track, last_start, last_span) = {
            let s = self.sched.lock().unwrap();
            match s.branches.last() {
                Some(b) if !b.followed => match b.span() {
                    Some(span) => (b.track, b.start_rt, span),
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

        // ...and so does its tempo, for the same reason: the speed is baked into
        // the pad offset and the fade at build time, so a branch built before the
        // measurement lands would play at 1.0x for its whole duration. Measuring
        // the *next* track while the current one plays is what makes this free —
        // there is a whole track's worth of time to do it in, and the only track
        // ever heard at the wrong speed is the one that was already playing when
        // the feature was switched on.
        //
        // This can only defer the follower, never lose it: `request_tempo`
        // always ends by recording an answer — a tag, a measurement, or "no
        // steady tempo" if the decode fails — and every answer comes back
        // through `Internal::Tempo`, which calls this again.
        if self.force_tempo() {
            if let Some(path) = self.track_path(next) {
                // The lock is taken into a `let` and released by the semicolon.
                // Testing it inline — `if !self.tempos.lock().unwrap().contains_key(..)`
                // — reads identically and deadlocks: a temporary MutexGuard in an
                // `if` condition lives to the end of the if **body**, and the body
                // calls `request_tempo`, which locks `tempos` again. std's Mutex
                // is not reentrant, so the GTK main loop stops dead: the process
                // stays alive, the window stops repainting, and the control API
                // accepts connections it will never answer. Nothing is printed,
                // because nothing panicked.
                let known = self.tempos.lock().unwrap().contains_key(&path);
                if !known {
                    self.request_tempo(next);
                    return;
                }
            }
        }

        let xf = self.crossfade.load(Ordering::SeqCst).min(last_span / 2);
        let start_rt = last_start + last_span.saturating_sub(xf);

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
    fn current_at(&self, pos: u64) -> Option<(usize, u64, u64, u64, f64)> {
        let s = self.sched.lock().unwrap();
        s.branches
            .iter()
            .filter(|b| b.start_rt <= pos)
            .max_by_key(|b| b.start_rt)
            .map(|b| (b.track, b.start_rt, b.len.unwrap_or(0), b.skip, b.speed))
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
                        let mut refade: Vec<(gst::Pad, u64, u64)> = Vec::new();
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
                                let len = Player::effective_len(&effective, b.trim_on, inner);
                                b.len = Some(len);
                                // The fade was written against a guess at the length
                                // (the container's duration). Now that the real one is
                                // known, redo it against the span this branch will
                                // actually play — on the clock, at this branch's speed.
                                refade.push((
                                    b.pad.clone(),
                                    b.start_rt,
                                    tempo::wall_clock(len.saturating_sub(b.skip), b.speed),
                                ));
                                if Some(b.track) == current {
                                    current_len = Some(len);
                                }
                            }
                            if let Some(len) = current_len {
                                s.current_len = len;
                            }
                        }
                        for (pad, start_rt, span) in refade {
                            player.apply_fade(&pad, start_rt, span);
                        }
                        player.schedule_following();
                    }

                    Internal::Tempo(track, found) => {
                        if let Some(path) = player.track_path(track) {
                            player.measuring.lock().unwrap().remove(&path);
                            // Recorded whatever the answer was, including "none".
                            // That is what stops a spoken-word track being
                            // decoded again every time it comes round.
                            player.tempos.lock().unwrap().insert(path.clone(), found);
                            let _ = player
                                .tx
                                .send_blocking(PlayerEvent::TempoMeasured { track, bpm: found });
                        }
                        // The follower may have been waiting on exactly this.
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

            if let Some((track, start_rt, len, skip, speed)) = player.current_at(global) {
                let changed = {
                    let mut s = player.sched.lock().unwrap();
                    let changed = s.announced != Some(track);
                    s.current = Some(track);
                    s.announced = Some(track);
                    s.current_start = start_rt;
                    // Follow the branch's own skip. Without this, advancing off a
                    // resumed track leaves the old resume offset in place and every
                    // position after it reads that much too high.
                    s.skip = skip;
                    // ...and its own speed, for the same reason: two tracks in one
                    // queue can be playing at different stretches, so the position
                    // conversion has to follow whichever one is audible.
                    s.speed = speed;
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
                    player.request_tempo_if_wanted(track);
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

/// One track's decode chain. `speed` is Force Tempo's stretch; at 1.0 nothing is
/// inserted and this is byte-for-byte the pipeline it always was.
fn build_branch(path: &Path, speed: f64) -> Result<(gst::Bin, gst::Pad, gst::Pad)> {
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

    // Force Tempo's stretcher goes **downstream of the probe**, and that ordering
    // is the whole design.
    //
    // The probe drops and retimestamps buffers by comparing their PTS against
    // trim points measured from the file — numbers in media time. Put the
    // stretcher before it and every one of those comparisons is against a
    // timebase that has been divided by the speed, so the leading-silence cut
    // lands in the wrong place and the interior cuts land somewhere else again.
    // Downstream, the probe keeps working in the units it was written in and the
    // stretcher rescales whatever survives.
    //
    // `pitch` (from soundtouch) rather than `scaletempo`: both preserve pitch
    // while changing tempo, but `pitch` exposes `tempo` as a plain multiplier on
    // a normal-rate stream, whereas `scaletempo` exists to compensate a pipeline
    // whose *rate* has been changed by a seek — which this engine never does,
    // because seeking is done with pad offsets.
    let exit = if tempo::is_unchanged(speed) {
        srcpad.clone()
    } else {
        let pitch = gst::ElementFactory::make("pitch")
            .property("tempo", speed as f32)
            .build()
            .map_err(|_| anyhow!("no pitch element — install gstreamer1.0-plugins-bad"))?;
        // The mixer must never see a caps change mid-stream, so the branch is
        // pinned back to exactly the format it was already producing rather than
        // trusting the stretcher to preserve it.
        let after = gst::ElementFactory::make("audioconvert").build()?;
        let pinned = gst::ElementFactory::make("capsfilter")
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
        bin.add_many([&pitch, &after, &pinned])?;
        gst::Element::link_many([&queue, &pitch, &after, &pinned])?;
        pinned.static_pad("src").ok_or_else(|| anyhow!("capsfilter has no src"))?
    };

    let ghost = gst::GhostPad::with_target(&exit)?;
    bin.add_pad(&ghost)?;

    // Two pads, and they are only the same one when nothing is stretching.
    //
    // The first is where buffers are edited. The real pad, not the ghost: a probe
    // on the ghost can drop a buffer, but a buffer *modified* there does not
    // survive the proxy hop to the mixer — the rewritten timestamps were silently
    // discarded. Probe the queue's own src pad instead, where the edit sticks.
    //
    // The second is where the branch actually ends, which is where "this track is
    // over" has to be observed. With a stretcher in the chain those are different
    // places: the queue has seen EOS while soundtouch is still holding the last
    // fraction of a second, and retiring the branch on the upstream EOS tears the
    // stretcher down before it has flushed — and before EOS ever reaches the
    // mixer, so downstream never finalises. It cost a WAV file whose audio was
    // perfectly correct and whose header claimed 12,173 seconds.
    Ok((bin, srcpad.upcast(), exit))
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

/// xorshift64. Not cryptographic and does not need to be — it decides what song
/// plays next. Pulled out of `shuffle_in_place` so the weighted shuffle can share
/// it and so the tests can seed it and get a reproducible order; a shuffle whose
/// distribution cannot be measured is a shuffle nobody can claim anything about.
struct Rng(u64);

impl Rng {
    fn from_clock() -> Self {
        Rng(std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15)
            | 1)
    }

    #[cfg(test)]
    fn seeded(seed: u64) -> Self {
        Rng(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Half-open (0, 1) — never exactly 0, because the weighted shuffle takes its
    /// logarithm.
    fn next_f64(&mut self) -> f64 {
        // 53 bits is the mantissa; +1 keeps it off zero.
        ((self.next_u64() >> 11) + 1) as f64 / (1u64 << 53) as f64
    }
}

fn shuffle_in_place(v: &mut [usize], rng: &mut Rng) {
    for i in (1..v.len()).rev() {
        v.swap(i, (rng.next_u64() % (i as u64 + 1)) as usize);
    }
}

/// Weighted shuffle without replacement: every track still appears exactly once,
/// but a heavier track is proportionally more likely to land early.
///
/// The method is Efraimidis–Spirakis: give each item the key `u^(1/w)` for
/// `u` uniform on (0,1), then sort by key descending. That draws exactly the same
/// distribution as repeatedly picking from the remaining items with probability
/// proportional to weight, in one O(n log n) pass instead of n quadratic draws.
///
/// It is computed in log space — `ln(u)/w`, sorted ascending, which orders
/// identically — because `u^(1/16)` on a 10,000-track queue pushes a lot of keys
/// into the same handful of floats near 1.0 and the sort stops distinguishing
/// them. The logarithm keeps them apart.
///
/// `weights` is indexed by **track**, while `v` holds track indices, so the two
/// are not parallel arrays; a zero or negative weight takes the smallest possible
/// key and sinks to the back, rather than dividing by zero.
fn weighted_shuffle_in_place(v: &mut [usize], weights: &[f64], rng: &mut Rng) {
    let mut keyed: Vec<(f64, usize)> = v
        .iter()
        .map(|&track| {
            let w = weights.get(track).copied().unwrap_or(1.0);
            let key = if w > 0.0 {
                rng.next_f64().ln() / w
            } else {
                f64::NEG_INFINITY
            };
            (key, track)
        })
        .collect();

    // Descending. ln(u) is negative, so dividing by a larger weight moves the key
    // *towards* zero — the heaviest tracks hold the largest keys, and largest
    // first is what the method selects. Sorting these ascending is an easy and
    // completely silent inversion: the queue still shuffles, it just prefers the
    // tracks you rated worst. Two tests below measure the direction for that
    // reason rather than only checking it is a permutation.
    keyed.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    for (slot, (_, track)) in keyed.into_iter().enumerate() {
        v[slot] = track;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the order the way `Queue::reorder` does, but from a seeded Rng so
    /// the result can be measured rather than asserted.
    fn favorites_order(ratings: &[u8], rng: &mut Rng) -> Vec<usize> {
        let weights: Vec<f64> = ratings.iter().map(|&r| weight_for(r)).collect();
        let mut order: Vec<usize> = (0..ratings.len()).collect();
        weighted_shuffle_in_place(&mut order, &weights, rng);
        order
    }

    /// The thing that would make this feature silently worthless: a weighted
    /// shuffle that drops or duplicates tracks. Every pass must still be a
    /// permutation — favorites shuffle is an ordering, not a filter.
    #[test]
    fn favorites_shuffle_is_still_a_permutation() {
        let ratings = [5, 0, 1, 3, 0, 5, 2, 0, 4, 0, 0, 1];
        let mut rng = Rng::seeded(0xDEAD_BEEF);
        for _ in 0..200 {
            let order = favorites_order(&ratings, &mut rng);
            let mut seen = order.clone();
            seen.sort_unstable();
            assert_eq!(seen, (0..ratings.len()).collect::<Vec<_>>());
        }
    }

    /// The claim the mode is named after, measured: over many passes, 5-star
    /// tracks must land meaningfully earlier than unrated ones, and 1-star
    /// tracks later. A uniform shuffle puts every mean at (n-1)/2 = 5.5 here, so
    /// a broken weighting shows up as three numbers that are all the same.
    #[test]
    fn favorites_shuffle_actually_prefers_favorites() {
        //            0  1  2  3  4  5  6  7  8  9 10 11
        let ratings = [5, 5, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1];
        let mut rng = Rng::seeded(0x5EED_1234);

        const PASSES: usize = 4000;
        let mut total_slot = vec![0usize; ratings.len()];
        for _ in 0..PASSES {
            for (slot, &track) in favorites_order(&ratings, &mut rng).iter().enumerate() {
                total_slot[track] += slot;
            }
        }
        let mean = |t: usize| total_slot[t] as f64 / PASSES as f64;

        let five = (mean(0) + mean(1)) / 2.0;
        let unrated = (2..=9).map(mean).sum::<f64>() / 8.0;
        let one = (mean(10) + mean(11)) / 2.0;

        // Printed, not just asserted: `cargo test -- --nocapture` shows the
        // actual spread, so a weighting that is technically in the right
        // direction but far too weak to hear is visible rather than passing.
        println!(
            "favorites shuffle, mean slot of 12 over {PASSES} passes \
             (uniform would be 5.50):  5-star {five:.2}   unrated {unrated:.2}   1-star {one:.2}"
        );

        assert!(
            five < unrated - 1.0,
            "5-star tracks must land earlier than unrated ones: {five:.2} vs {unrated:.2}"
        );
        assert!(
            unrated < one - 1.0,
            "unrated must land earlier than 1-star: {unrated:.2} vs {one:.2}"
        );
        // Uniform shuffle would put all three at 5.5. If the top group is not
        // clearly below that, the weighting is not doing anything.
        assert!(five < 4.0, "5-star mean slot {five:.2} is barely better than uniform (5.5)");
    }

    /// Weighted, not filtered. A 1-star track in a queue full of 5-star ones must
    /// still come up sometimes — and in particular must sometimes come up *first*,
    /// or the mode has quietly become "play only favorites".
    #[test]
    fn a_disliked_track_is_rare_not_banned() {
        let mut ratings = vec![5u8; 8];
        ratings.push(1); // track 8
        let mut rng = Rng::seeded(0xFEED_FACE);

        let mut first = 0;
        for _ in 0..4000 {
            if favorites_order(&ratings, &mut rng)[0] == 8 {
                first += 1;
            }
        }
        assert!(first > 0, "a 1-star track must not be unreachable");
        // 1 against eight 16s: 1/129 ≈ 0.8%, so ~31 of 4000. Anything near
        // uniform (1/9 ≈ 444) means the weights are not being applied.
        assert!(
            first < 150,
            "a 1-star track came first {first} times in 4000 — the weighting is too weak"
        );
    }

    /// Plain shuffle must stay plain: with the mode off ratings change nothing,
    /// which is what makes Favorites a *separate* mode rather than a tilt applied
    /// to every shuffle.
    #[test]
    fn plain_shuffle_ignores_ratings_entirely() {
        let mut rng = Rng::seeded(7);
        let mut order: Vec<usize> = (0..12).collect();
        let mut total_slot = vec![0usize; 12];
        const PASSES: usize = 4000;
        for _ in 0..PASSES {
            order = (0..12).collect();
            shuffle_in_place(&mut order, &mut rng);
            for (slot, &track) in order.iter().enumerate() {
                total_slot[track] += slot;
            }
        }
        for track in 0..12 {
            let mean = total_slot[track] as f64 / PASSES as f64;
            assert!(
                (mean - 5.5).abs() < 0.6,
                "plain shuffle is not uniform: track {track} mean slot {mean:.2}"
            );
        }
    }

    #[test]
    fn shuffle_mode_cycles_through_all_three_and_back() {
        let mut m = Shuffle::Off;
        m = m.cycle();
        assert_eq!(m, Shuffle::On);
        m = m.cycle();
        assert_eq!(m, Shuffle::Favorites);
        m = m.cycle();
        assert_eq!(m, Shuffle::Off);
    }

    /// The string form is what lands in state.json, so an unknown value — a
    /// hand-edited config, or one written by a future build — must read as Off
    /// rather than panic.
    #[test]
    fn shuffle_mode_survives_a_round_trip_through_text() {
        for mode in [Shuffle::Off, Shuffle::On, Shuffle::Favorites] {
            assert_eq!(Shuffle::from_str(mode.as_str()), mode);
        }
        assert_eq!(Shuffle::from_str("weighted-by-mood"), Shuffle::Off);
        assert_eq!(Shuffle::from_str(""), Shuffle::Off);
    }

    /// Unrated must not be treated as the worst possible rating. It sits level
    /// with 2 stars: not yet judged, not disliked.
    #[test]
    fn unrated_outranks_one_star() {
        assert!(weight_for(0) > weight_for(1));
        assert_eq!(weight_for(0), weight_for(2));
        assert!(weight_for(5) > weight_for(4));
        // Anything out of range lands on the neutral weight rather than skewing.
        assert_eq!(weight_for(200), weight_for(0));
    }

    /// Degenerate inputs the GUI can genuinely produce: an empty queue, and a
    /// single track.
    #[test]
    fn empty_and_single_queues_do_not_panic() {
        let mut rng = Rng::seeded(1);
        let mut empty: Vec<usize> = Vec::new();
        weighted_shuffle_in_place(&mut empty, &[], &mut rng);
        assert!(empty.is_empty());

        let mut one = vec![0usize];
        weighted_shuffle_in_place(&mut one, &[16.0], &mut rng);
        assert_eq!(one, vec![0]);
    }
}

#[cfg(test)]
mod cue_tests {
    use super::*;

    /// A real `Player` with the audio sink swapped for a `fakesink`, so this runs
    /// headless like the rest of the suite.
    fn headless_player() -> Arc<Player> {
        // Before the element is built, not after: `Player::with_sink` inits too,
        // but the sink is constructed by the caller and needs a live registry.
        gst::init().expect("gstreamer must initialise");
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .expect("fakesink is in gstreamer-core");
        Player::with_sink(Some(sink)).expect("engine must start")
    }

    fn queued(paths: &[&str]) -> Vec<QueuedTrack> {
        paths
            .iter()
            .map(|p| QueuedTrack {
                path: PathBuf::from(p),
                duration_nanos: 180_000_000_000,
                rating: 0,
            })
            .collect()
    }

    /// Everything about the resume cue, in **one** test on purpose.
    ///
    /// `Player` installs a bus watch on the glib main context, and a main context
    /// is owned by the first thread to acquire it. `cargo test` gives every test
    /// its own thread, so a second test that builds a `Player` panics with
    /// "thread default main context already acquired by another thread" — and
    /// which one panics is down to scheduling, so it fails intermittently. One
    /// test, one thread, as many `Player`s as it likes.
    ///
    /// The defect being guarded, found by operating the shipped app rather than
    /// by any test: the resume point lived in the GTK front-end's `Ui`, which
    /// `mpris.rs` cannot see. The play **button** resumed where the last session
    /// stopped and a media key or a lock-screen Play did **not** — it called
    /// `play_index(0)`, started the queue from the top and threw the resume point
    /// away. One cue, on the `Player`, is what makes every caller agree.
    #[test]
    fn the_resume_cue_belongs_to_the_player() {
        let player = headless_player();
        player.set_tracks(queued(&["/music/a.mp3", "/music/b.mp3", "/music/c.mp3"]));

        assert_eq!(player.cued(), None, "a fresh player has nothing cued");

        player.set_cued(Some((2, 87_500_000_000)));
        assert_eq!(
            player.cued(),
            Some((2, 87_500_000_000)),
            "the cue must be readable WITHOUT consuming it — saving the session \
             while still cued depends on that"
        );
        assert_eq!(
            player.cued(),
            Some((2, 87_500_000_000)),
            "reading twice must still not consume it"
        );

        player.clear_cue();
        assert_eq!(player.cued(), None);

        // A cue against a queue that has been replaced addresses a different
        // song, so loading a new source must drop it.
        player.set_cued(Some((1, 12_000_000_000)));
        player.set_tracks(queued(&["/other/x.mp3", "/other/y.mp3"]));
        player.clear_cue();
        assert_eq!(player.cued(), None);

        // `play()` on an empty queue is an error, not a panic and not a silent
        // no-op — the control API turns it into a 409.
        let empty = headless_player();
        assert!(empty.play().is_err());
    }
}

//! Force Tempo's arithmetic: given what a track's tempo *is* and what you want
//! it to be, how fast should it play?
//!
//! The naive answer — `speed = target / source` — is wrong twice over, and both
//! failures are audible:
//!
//!   1. **Octave ambiguity.** A track counted at 70 BPM and a track counted at
//!      140 BPM can be the same felt pace; 70 is simply the half-time reading of
//!      it. Stretching such a track to 2.0x would double a pace that already
//!      matched. So the reading is folded by factors of two — but only when a
//!      folded reading lands *near* the target ([`NEAR_TOLERANCE`]), near enough
//!      that calling it the same tempo is a description rather than an excuse.
//!      A 95 BPM track is not a 190 BPM track that someone wrote down slowly,
//!      and is not treated as one.
//!   2. **Unbounded stretch.** Time-stretching is transparent at 1.1x and a
//!      novelty at 1.8x. `max_change_percent` is the ceiling the user sets, and
//!      the answer is clamped to it rather than allowed to be absurd.
//!
//! And one preference on top, because of what the feature is *for*: with
//! `only_faster` set (the default), a track already at or above the target is
//! left completely alone. The point of Force Tempo is a workout with no slow
//! patches in it — slowing a fast track down to hit a number would be the
//! feature working against its own reason for existing.
//!
//! No GStreamer in here: it is arithmetic with exactly right answers, so it is
//! tested as arithmetic.
//!
//! # Media time is not wall-clock time
//!
//! This module is also where the player stops being able to pretend those are
//! the same number. Everywhere else in the engine a nanosecond is a nanosecond,
//! because the only speed was 1.0. Once a branch plays at 1.3x, the last six
//! seconds *of the track* are 4.6 seconds *of listening*, and the mixer timeline
//! is measured in the second of those. [`wall_clock`] and [`media`] are the only
//! two places that conversion is allowed to happen.

/// Playback speed meaning "leave it alone".
pub const UNCHANGED: f64 = 1.0;

/// How far a halved or doubled reading may land from the target and still be
/// accepted as the same tempo. 15% is about the width of the window in which two
/// tempos are felt as one pace rather than as a change.
pub const NEAR_TOLERANCE: f64 = 0.15;

/// A floor under any speed used as a divisor, so nothing downstream can divide
/// by zero.
pub const MIN_SPEED: f64 = 0.05;

/// The range the UI offers, and the range the API accepts. Outside it the
/// feature stops being "bring this playlist to one pace" and starts being a
/// sound effect.
pub const MIN_TARGET_BPM: u32 = 60;
pub const MAX_TARGET_BPM: u32 = 200;

/// The ceiling on the ceiling. Above ~30% a stretched track begins to sound
/// stretched rather than fast; 50% is as far as the control goes.
pub const MAX_STRETCH_PERCENT: u32 = 50;

/// Defaults, chosen for the case the feature exists to serve — see the module
/// docs. Same numbers Lull ships.
pub const DEFAULT_TARGET_BPM: u32 = 140;
pub const DEFAULT_MAX_CHANGE_PERCENT: u32 = 30;
pub const DEFAULT_ONLY_FASTER: bool = true;

/// The speed to play a track at.
///
/// * `source_bpm` — the track's own tempo; 0 or less means unknown, and unknown
///   means untouched.
/// * `target_bpm` — the tempo the user asked for.
/// * `max_change_percent` — the largest stretch allowed, as a percentage
///   (30 → up to 1.30x, and down to 1/1.30 when `only_faster` is off).
/// * `only_faster` — never play a track slower than it was recorded.
pub fn speed_for(
    source_bpm: f64,
    target_bpm: u32,
    max_change_percent: u32,
    only_faster: bool,
) -> f64 {
    if !source_bpm.is_finite() || source_bpm <= 0.0 || target_bpm == 0 {
        return UNCHANGED;
    }

    let target = target_bpm as f64;
    let reading = resolve_reading(source_bpm, target);
    let raw = target / reading;

    if only_faster && raw < UNCHANGED {
        return UNCHANGED;
    }

    let ceiling = 1.0 + max_change_percent as f64 / 100.0;
    let floor = if only_faster { UNCHANGED } else { 1.0 / ceiling };
    raw.clamp(floor, ceiling)
}

/// Which reading of `source_bpm` to believe: itself, its double, or its half.
///
/// A folded reading wins only if it lands inside [`NEAR_TOLERANCE`] of the
/// target — that is the whole guard against turning "this track is slower than
/// you asked for" into "this track is secretly already the right speed". Where
/// nothing qualifies, the detector's own number stands and the stretch (or the
/// clamp) is honest about what it is doing.
///
/// Distance is measured in log space, so "15% away" means the same thing in both
/// directions. It is not: 140 → 161 is +15%, but 140 → 119 is −15% and the ratio
/// back is 1.176. A linear test would accept one and refuse its mirror image.
pub fn resolve_reading(source_bpm: f64, target_bpm: f64) -> f64 {
    if !source_bpm.is_finite() || source_bpm <= 0.0 || target_bpm <= 0.0 {
        return source_bpm;
    }
    let limit = (1.0 + NEAR_TOLERANCE).ln();
    let mut best = source_bpm;
    let mut best_distance = f64::MAX;
    for candidate in [source_bpm, source_bpm * 2.0, source_bpm / 2.0] {
        let distance = (target_bpm / candidate).ln().abs();
        if distance <= limit && distance < best_distance {
            best = candidate;
            best_distance = distance;
        }
    }
    best
}

/// How long a stretch of *track* takes on the *clock*, at `speed`.
///
/// At 1.3x the last six seconds of a track are 4.6 seconds of listening. A fade
/// timed against the wrong one either starts late and gets cut off by the
/// transition, or starts early and runs long.
pub fn wall_clock(media_ns: u64, speed: f64) -> u64 {
    (media_ns as f64 / speed.max(MIN_SPEED)).round() as u64
}

/// The inverse: how much *track* a stretch of clock covers. Used to turn a
/// position on the mixer timeline back into a position in the song, which is
/// what the seek bar and every consumer of `/api/status` expect to see.
pub fn media(wall_ns: u64, speed: f64) -> u64 {
    (wall_ns as f64 * speed.max(MIN_SPEED)).round() as u64
}

/// The tempo you will actually hear: the track's own, played at `speed`.
pub fn effective_bpm(source_bpm: f64, speed: f64) -> f64 {
    source_bpm * speed
}

/// `1.09x` — what the settings panel and the API report. Two decimals is finer
/// than the ear.
pub fn format_speed(speed: f64) -> String {
    format!("{speed:.2}x")
}

/// Whether a speed is close enough to 1.0 that inserting a stretcher would cost
/// CPU and buy nothing audible. Half a percent is about 0.7 BPM at 140.
pub fn is_unchanged(speed: f64) -> bool {
    (speed - UNCHANGED).abs() < 0.005
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-6, "{a} != {b}");
    }

    #[test]
    fn unknown_tempo_is_never_touched() {
        approx(speed_for(0.0, 140, 30, true), UNCHANGED);
        approx(speed_for(-1.0, 140, 30, true), UNCHANGED);
        approx(speed_for(f64::NAN, 140, 30, true), UNCHANGED);
        approx(speed_for(120.0, 0, 30, true), UNCHANGED);
    }

    #[test]
    fn a_slow_track_is_brought_up_to_the_target() {
        // 128 -> 140 is 1.09375, inside a 30% ceiling.
        approx(speed_for(128.0, 140, 30, true), 140.0 / 128.0);
    }

    /// The fold is applied *before* the clamp, and this is the pair that proves
    /// it: 80 -> 140 looks like a 1.75x stretch, but 160 is inside tolerance of
    /// 140, so the track is already at the asked-for pace and is left alone.
    #[test]
    fn the_fold_is_checked_before_the_ceiling() {
        approx(speed_for(80.0, 140, 30, true), UNCHANGED);
    }

    #[test]
    fn a_track_that_cannot_reach_the_target_is_clamped_not_refused() {
        // 95 -> 140 is 1.47x; neither 190 nor 47.5 is near 140, so no fold.
        let s = speed_for(95.0, 140, 30, true);
        approx(s, 1.30);
        assert!(s < 140.0 / 95.0, "must be clamped below the raw ratio");
    }

    /// The case the whole fold exists for.
    #[test]
    fn a_half_time_reading_is_left_alone_rather_than_doubled() {
        // 70 in double time IS 140. Stretching to 2.0x would be absurd.
        approx(speed_for(70.0, 140, 30, true), UNCHANGED);
    }

    /// ...and the case it must NOT excuse. This is the pair that makes the
    /// tolerance a description rather than a loophole.
    #[test]
    fn a_merely_slow_track_is_not_excused_as_a_secretly_fast_one() {
        // 95*2 = 190. In ratio terms 190/140 = 1.357 and 140/95 = 1.474, so a
        // naive "closest in ratio" rule would fold it and leave the track alone.
        assert_eq!(resolve_reading(95.0, 140.0), 95.0);
        assert!(speed_for(95.0, 140, 30, true) > 1.0, "95 BPM must be stretched");
    }

    #[test]
    fn a_double_time_reading_folds_down() {
        // 260 read for a 130 track, target 140: 130 is within 15% of 140.
        approx(resolve_reading(260.0, 140.0), 130.0);
    }

    /// A reading 14% above the target and one 14% below it (in ratio) must both
    /// fold. A linear "within 15% of 140" test accepts 159.6 and refuses 122.8,
    /// because 140 - 122.8 is 17.2 — it would treat a tempo and its mirror image
    /// differently for no reason a listener could hear.
    #[test]
    fn tolerance_is_symmetric_in_log_space() {
        let above = 140.0 * 1.14;
        let below = 140.0 / 1.14;
        approx(resolve_reading(above * 2.0, 140.0), above);
        approx(resolve_reading(below / 2.0, 140.0), below);
    }

    /// Nothing outside tolerance folds, in either direction.
    #[test]
    fn a_reading_far_from_the_target_keeps_its_own_octave() {
        assert_eq!(resolve_reading(210.0, 140.0), 210.0);
        assert_eq!(resolve_reading(95.0, 140.0), 95.0);
    }

    #[test]
    fn only_faster_leaves_a_fast_track_completely_alone() {
        approx(speed_for(175.0, 140, 30, true), UNCHANGED);
    }

    #[test]
    fn without_only_faster_a_fast_track_is_slowed_to_meet_the_target() {
        // 175 -> 140 is 0.8x, and 1/1.3 = 0.769, so 0.8 survives the clamp.
        approx(speed_for(175.0, 140, 30, false), 0.8);
    }

    #[test]
    fn slowing_down_is_clamped_by_the_same_ceiling_inverted() {
        // 300 -> 140 would be 0.467x. Folded: 150 is within 15% of 140, so it
        // resolves to 150 and only needs 0.933x.
        approx(speed_for(300.0, 140, 30, false), 140.0 / 150.0);
        // A genuinely unfoldable fast track hits the floor.
        let s = speed_for(210.0, 140, 30, false);
        approx(s, 1.0 / 1.30);
    }

    /// The conversion that Force Tempo forces the rest of the engine to learn.
    #[test]
    fn six_seconds_of_track_is_less_than_six_seconds_of_clock_when_sped_up() {
        assert_eq!(wall_clock(6_000_000_000, 1.3), 4_615_384_615);
        assert_eq!(wall_clock(6_000_000_000, 1.0), 6_000_000_000);
    }

    #[test]
    fn media_and_wall_clock_are_inverses() {
        for &speed in &[0.8f64, 1.0, 1.09375, 1.3] {
            let media_ns = 197_000_000_000u64;
            let there = wall_clock(media_ns, speed);
            let back = media(there, speed);
            assert!(
                (back as i64 - media_ns as i64).abs() <= 1,
                "round trip at {speed}x lost {} ns",
                (back as i64 - media_ns as i64).abs()
            );
        }
    }

    #[test]
    fn nothing_divides_by_zero() {
        assert_eq!(wall_clock(1_000_000_000, 0.0), 20_000_000_000);
        assert_eq!(media(1_000_000_000, 0.0), 50_000_000);
    }

    #[test]
    fn effective_bpm_is_what_you_hear() {
        approx(effective_bpm(128.0, 140.0 / 128.0), 140.0);
    }

    #[test]
    fn a_speed_within_half_a_percent_counts_as_unchanged() {
        assert!(is_unchanged(1.0));
        assert!(is_unchanged(1.004));
        assert!(!is_unchanged(1.01));
    }

    #[test]
    fn formats_to_two_decimals() {
        assert_eq!(format_speed(1.09375), "1.09x");
        assert_eq!(format_speed(1.0), "1.00x");
    }
}

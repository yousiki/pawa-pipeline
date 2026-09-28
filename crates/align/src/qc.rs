//! Telemetry quality control for a captured episode's action streams.
//!
//! WHY THIS EXISTS. In August 2026 a 25-scenario capture shipped, was reviewed by eye, and turned
//! out to carry four defects that were all visible in the recorded telemetry the whole time: 59.9%
//! of frames idle with single frozen stretches up to 114.5 s, one bot in `gap_jump` that moved for
//! 13 frames out of 2,583 while nobody noticed, follower bots emitting near-identical action tuples,
//! and a sprint bit that was a deterministic function of the navigation state. A human watching
//! video is the wrong instrument for any of those; every one is a counting problem over the
//! `<pov>-actions.jsonl` rows this module consumes as [`ActionFrame`].
//!
//! WHAT THIS IS NOT. It reads action rows only — key bits, camera deltas, positions — plus
//! caller-supplied per-frame luma samples ([`check_pov_darkness`]; the ffmpeg call that
//! produces them lives in the render crate, this module stays pure). It never judges how anything
//! looks. Whether footage is *good* stays a human question; this only refuses to ship an episode
//! whose recorded actions are provably degenerate — or whose pixels are provably absent.
//!
//! The thresholds are deliberately loose. They are meant to catch "this episode is broken" (a bot
//! that never moved, an hour of statue), not to enforce a taste in gameplay. A caller that wants
//! stricter gates passes its own [`Thresholds`].
//!
//! # How the action rows are read
//!
//! * [`ActionFrame`] carries no velocity — it is derivable — so the travel bearing comes from the
//!   horizontal position difference between consecutive rows, in blocks per tick. That is exact as
//!   long as rows are dense and consecutive, as the capture runtime writes them; gaps in the tick
//!   sequence are not compensated for.
//! * "Moving" is read off the four movement key bits.
//! * The module answers the per-episode question only: it takes already-parsed POVs. Episode
//!   discovery and corpus-level checks (intersecting `dead_keys` across episodes to enforce
//!   `require_keys`) belong to the caller; `dead_keys` is always reported so it can do that.
//! * The tick rate is a parameter. Thresholds expressed in seconds scale with it (a sustained
//!   forward run is one second of ticks; the idle stretch is reported in seconds).
//!   `SHORT_RUN_FRAMES` does not: it is a claim about ticks, measured in ticks.
//! * Metric values are rounded where they are stored — 4 decimals for fractions and agreements, 2
//!   for seconds. Failure messages are formatted from the unrounded values.

use std::collections::{BTreeMap, BTreeSet};

use pawa_schema::capture::ActionFrame;
use pawa_schema::dataset::{DegeneracyMetrics, MovementKeys};

/// The seven key bits. The order is load-bearing only in that it must be the same for every POV:
/// pair agreement compares packed action tuples.
const ALL_KEYS: [&str; 7] = [
    "forward", "back", "left", "right", "jump", "sneak", "sprint",
];
/// How many of [`ALL_KEYS`] are movement keys — the leading run of the array.
const MOVE_KEY_COUNT: usize = 4;
/// A press this short cannot have taken effect: 1-4 tick movement bouts moved the player a median
/// of 0.43 blocks in the 2026-08 corpus, and 63.2% of them moved under half a block. A tick count,
/// not a duration, which is why it does not scale with the tick rate.
const SHORT_RUN_FRAMES: usize = 10;

// --- displacement-based stillness ---------------------------------------------------------------
// Everything above reads keys; a bot holding W into a tree trunk scores as "moving" there. The
// first collected corpus passed every key-based check while reviewers saw statues, which is why these
// metrics exist: they read POSITION. A frame is frozen when net 3D displacement over the trailing
// one-second window is under [`FROZEN_DISPLACEMENT_BLOCKS`] and the frame carries no discrete
// action (a bot placing or mining where it stands is working, not frozen — `build_house` does 80 s
// stretches of exactly that). The vertical term is taken between the windows' rolling floor
// (minimum y over the trailing second), so climbing a pillar counts as displacement while jumping
// in place — the classic stuck-against-a-wall escape reflex — does not.

/// Net 3D displacement (blocks per second of window) under which a frame is frozen. Sneaking
/// travels ~1.3 blocks/s and the slowest deliberate gaits in the 2026-08 corpus stayed above 1.
const FROZEN_DISPLACEMENT_BLOCKS: f64 = 0.5;
/// Movement keys held through a frozen stretch at least this long = "stuck": the scripted intent
/// says walk, the world says no. Short contacts are normal pathing; 5 s of shove is a defect.
const STUCK_MIN_RUN_S: f64 = 5.0;

// --- camera/travel independence ---------------------------------------------------------------
// `yaw` and `d_yaw` are RADIANS in the recorded stream, not degrees; `yaw` runs ±π and a typical
// turning frame carries `d_yaw` ≈ 0.06. Nothing above needs the unit (idleness only asks whether
// the camera moved at all), which is why it went unstated until something had to do trigonometry.

/// Horizontal speed (blocks/tick) below which "the direction of travel" is not a meaningful thing.
const CAMERA_SPEED_FLOOR: f64 = 0.05;
/// Angular error (radians) below which "the camera turned toward travel" is noise, not a choice.
const CAMERA_ANGLE_EPS: f64 = 0.02;
/// Eligible frames a POV needs before its predictability is worth reporting. The smallest 2026-08
/// episode offered 243.
const MIN_CAMERA_FRAMES: u64 = 100;
/// Sustained forward runs a POV needs before the sprint bit's predictability is worth reporting.
const MIN_SUSTAINED_FORWARD_RUNS: u64 = 20;
/// Movement-key presses a POV needs before its short-press share is evidence of chatter rather
/// than noise: a digging-trade bot that walked eight times all episode flips this metric on the
/// length of individual presses.
const MIN_MOVE_KEY_RUNS: u64 = 20;

// --- video darkness -----------------------------------------------------------------------------
// The one degeneracy signal the action stream cannot carry: in an early collection round bots walked into unlit
// caves and recorded minutes of near-black default POV — night_vision was applied and the footage
// was still black — while every key, camera and displacement metric read as a healthy, busy bot.

/// Mean frame luma (`signalstats` YAVG, 0-255) under which a sampled frame is "near-black".
/// Limited-range video floors true black at Y=16, so 24 means "within 8 codes of black" —
/// unreadably dark, not merely dim. Measured on an early-round `action_sampler-n01-a` p00 video
/// (a bot that walked into an unlit cave): the unreadable stretches sit at YAVG 19.8-23.9 while
/// the dimmest readable footage in the same episode starts at 29, so 24 splits the two cleanly.
pub const DARK_LUMA_MAX: f64 = 24.0;

/// Per-episode acceptance limits. Every field is an upper or lower bound on a counted quantity.
#[derive(Clone, Debug, PartialEq)]
pub struct Thresholds {
    pub max_idle_frac: f64,
    pub max_idle_run_s: f64,
    pub min_moving_frames_per_pov: u64,
    /// Of forward presses that lasted at least a second, the share allowed to all agree on sprint.
    /// 1.0 disables the check; the shipped corpus scored 0.981 and that is the defect.
    pub max_sustained_sprint_agreement: f64,
    /// Share of a movement key's presses allowed to be shorter than `SHORT_RUN_FRAMES`.
    pub max_short_run_frac: f64,
    /// Action-tuple agreement allowed between two players over frames where both are active.
    pub max_pair_agreement_active: f64,
    /// Of the frames where a moving bot turned its camera, the share allowed to agree on whether
    /// the turn went toward the direction of travel. Read it as predictability, not direction: 0.5
    /// is a camera that owes nothing to the path, 1.0 is one whose every turn is inferable from it.
    /// Taken in the same `max(agree, disagree)` form as the sprint check above, and for the same
    /// reason — `group_orbit` turned AWAY from travel on 86.0% of its frames, which is exactly as
    /// predictable as turning toward it and would otherwise have scored as the cleanest episode in
    /// the corpus. The 2026-08 corpus scored 0.738 pooled over 38,650 frames, per-episode median
    /// 0.854, range 0.520 (`pvp`) to 0.980 (`independent_build_tower`); 0.95 flags two of its
    /// twenty-five.
    pub max_camera_travel_agreement: f64,
    /// Longest stretch (seconds) a POV may go without net displacement. Displacement-based, so
    /// key-holding does not launder stillness the way it does for `max_idle_run_s`.
    pub max_frozen_run_s: f64,
    /// Total seconds per POV allowed inside stuck stretches (movement keys held, no displacement,
    /// for `STUCK_MIN_RUN_S` or longer).
    pub max_stuck_total_s: f64,
    /// Keys that MUST appear somewhere in this episode. Empty by default on purpose: one episode is
    /// the wrong scope for bit coverage — `straight_line_walk` has no honest reason to sneak, while
    /// a corpus in which no episode ever sneaks (the 2026-08 case: zero presses in 487,160 frames)
    /// is carrying an inert conditioning bit. [`EpisodeQcOutcome::dead_keys`] is always reported so
    /// a corpus-level caller can intersect it across episodes; only an explicit list here makes it
    /// a failure.
    pub require_keys: Vec<String>,
    /// Share of sampled video frames allowed to be near-black (mean luma under [`DARK_LUMA_MAX`]).
    /// The one degeneracy bound that reads pixels — judged in [`check_pov_darkness`] over luma
    /// samples the finalize stage extracts from the default projection. Tightened
    /// from the lenient 0.5 launch value: a quarter of an episode filming the inside
    /// of a cave is already weak training signal, and scenario scripts are expected to keep bots
    /// out of unlit caves rather than lean on this gate. Uniform across scenarios.
    pub max_dark_frame_frac: f64,
    /// Longest continuous near-black stretch (seconds) a POV's video may carry. 20 s still passes
    /// brief cave passages and night transitions (the known `action_sampler-n01-a` dive's longest
    /// stretch was 22 s — such an episode now correctly fails) while refusing the early-round
    /// failure mode this exists for: a bot that settles into an unlit cave and records minutes
    /// of black.
    pub max_dark_run_s: f64,
    /// Spread (max − min, ticks) allowed between the shipped files' first-row `worldAgeTicks`.
    /// The finalize trim cuts every POV to the same world-tick span, so the spread is 0 by
    /// construction; anything above it means the trim regressed (e.g. back to frame-index
    /// windows — sharded captures start their recordings ~14 ticks apart, and a 16-POV episode
    /// measured 51 ticks of start stagger while every per-POV alignment validated
    /// cleanly). Judged in [`check_pov_anchor_spread`] at finalize, on the post-trim rows.
    pub max_pov_anchor_spread_ticks: i64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            max_idle_frac: 0.75,
            max_idle_run_s: 20.0,
            min_moving_frames_per_pov: 200,
            max_sustained_sprint_agreement: 0.95,
            max_short_run_frac: 0.35,
            max_pair_agreement_active: 0.80,
            max_camera_travel_agreement: 0.95,
            max_frozen_run_s: 20.0,
            max_stuck_total_s: 5.0,
            require_keys: Vec::new(),
            max_dark_frame_frac: 0.25,
            max_dark_run_s: 20.0,
            max_pov_anchor_spread_ticks: 2,
        }
    }
}

impl Thresholds {
    /// Defaults tiered by catalog metadata. The displacement-based
    /// limits are never relaxed — they are the hard floor everything else exists to serve.
    ///
    /// * `world_edits` scenarios do stationary work (`build_house` laid blocks through an 84 s
    ///   stretch that key-based idle reads as a statue), so the three KEY-based bounds loosen to
    ///   where the first corpus's genuinely-working builders land, while a truly frozen bot still trips
    ///   `max_frozen_run_s`.
    /// * `pursuit` scenarios stare at their target while travelling — a camera that restates the
    ///   path is correct semantics there, so that check is disabled (1.0).
    pub fn for_scenario(name: &str) -> Self {
        let mut thresholds = Self::default();
        if let Some(info) = pawa_schema::catalog::scenario(name) {
            if info.world_edits {
                thresholds.min_moving_frames_per_pov = 50;
                thresholds.max_idle_frac = 0.90;
                thresholds.max_idle_run_s = 90.0;
            }
            if info.family == "pursuit" {
                thresholds.max_camera_travel_agreement = 1.0;
            }
            if info.family == "combat" {
                // Melee footwork is legitimately twitchy — clinch backoffs and reach-edge
                // approaches are sub-second presses by design. pvp at four bots measured
                // 36-63% short presses while every displacement metric was healthy.
                thresholds.max_short_run_frac = 0.65;
            }
        }
        thresholds
    }
}

/// One POV's measurements plus the threshold violations they produced.
#[derive(Clone, Debug, PartialEq)]
pub struct PovQcOutcome {
    pub metrics: DegeneracyMetrics,
    pub failures: Vec<String>,
}

/// The episode verdict: every POV's outcome, the two questions only a multi-POV episode can answer
/// (are the bots clones? is any key dead everywhere?), and the pooled camera figure.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EpisodeQcOutcome {
    /// Every violation found, per-POV ones included, in the order they were detected.
    pub failures: Vec<String>,
    /// Keys pressed by no POV in this episode, sorted.
    pub dead_keys: Vec<String>,
    pub pair_agreement_active: f64,
    /// Camera predictability pooled over POVs rather than averaged, so POVs weigh by the evidence
    /// they carry — which is how the 2026-08 baseline of 0.738 over 38,650 frames was taken.
    pub camera_travel_agreement: f64,
    pub povs: BTreeMap<String, PovQcOutcome>,
}

impl EpisodeQcOutcome {
    pub fn ok(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Raw counts for one POV. [`DegeneracyMetrics`] stores the derived, rounded values; the pooled
/// episode-level camera figure needs the counts themselves.
#[derive(Default)]
struct PovCounts {
    frames: u64,
    idle: u64,
    longest_idle: u64,
    moving: u64,
    forward_runs_sustained: u64,
    forward_runs_sustained_sprint: u64,
    move_key_runs: u64,
    move_key_runs_short: u64,
    camera_frames: u64,
    camera_toward_travel: u64,
    frozen: u64,
    longest_frozen: u64,
    stuck: u64,
    keys_never_pressed: BTreeSet<String>,
}

impl PovCounts {
    fn idle_frac(&self) -> f64 {
        ratio(self.idle, self.frames)
    }

    fn longest_idle_s(&self, tick_rate_hz: u32) -> f64 {
        self.longest_idle as f64 / f64::from(tick_rate_hz.max(1))
    }

    fn short_run_frac(&self) -> f64 {
        ratio(self.move_key_runs_short, self.move_key_runs)
    }

    /// How predictable this POV's camera turns are from its direction of travel, in [0.5, 1].
    fn camera_travel_agreement(&self) -> f64 {
        agreement(self.camera_toward_travel, self.camera_frames)
    }

    fn frozen_frac(&self) -> f64 {
        ratio(self.frozen, self.frames)
    }

    fn longest_frozen_s(&self, tick_rate_hz: u32) -> f64 {
        self.longest_frozen as f64 / f64::from(tick_rate_hz.max(1))
    }

    fn stuck_total_s(&self, tick_rate_hz: u32) -> f64 {
        self.stuck as f64 / f64::from(tick_rate_hz.max(1))
    }
}

/// One POV's per-frame evidence for the pairwise comparison: the packed action tuple and whether
/// the frame was idle.
struct PovTrace {
    tuples: Vec<u16>,
    idle: Vec<bool>,
}

/// The share of the majority answer: 0.5 when a binary outcome is balanced, 1.0 when it never
/// varies. Both the sprint and the camera checks ask "how predictable was this", never "which way".
fn agreement(hits: u64, total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    hits.max(total - hits) as f64 / total as f64
}

fn ratio(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64
    }
}

/// An angle folded into (-π, π], so "which way is shorter" is just its sign.
fn wrap(radians: f64) -> f64 {
    (radians + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU) - std::f64::consts::PI
}

/// The yaw a bot would hold to face its own horizontal velocity, in the stream's own convention.
///
/// Verified against the shipped corpus rather than derived: over 793 frames of unstrafed forward
/// walking in `straight_line_walk`, this differs from the recorded `yaw` by a median of 1.4°.
fn travel_bearing(vx: f64, vz: f64) -> f64 {
    (-vx).atan2(vz)
}

/// `(start, length)` of every consecutive `true` run.
fn runs(flags: &[bool]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (index, &flag) in flags.iter().enumerate() {
        match (flag, start) {
            (true, None) => start = Some(index),
            (false, Some(begin)) => {
                out.push((begin, index - begin));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start {
        out.push((begin, flags.len() - begin));
    }
    out
}

fn key_bits(keys: &MovementKeys) -> [bool; ALL_KEYS.len()] {
    [
        keys.forward,
        keys.back,
        keys.left,
        keys.right,
        keys.jump,
        keys.sneak,
        keys.sprint,
    ]
}

/// Round a stored metric to `decimals` places. `f64::round` breaks ties away from zero rather than
/// to even; the difference can only reach the last stored digit of a value that is never compared
/// against a threshold after rounding.
fn round_to(value: f64, decimals: i32) -> f64 {
    let scale = 10f64.powi(decimals);
    (value * scale).round() / scale
}

fn pct(value: f64) -> String {
    format!("{:.1}%", value * 100.0)
}

fn pct0(value: f64) -> String {
    format!("{:.0}%", value * 100.0)
}

/// Count one POV's action stream: its raw counts, per-frame packed action tuples, and idle mask.
fn read_pov(frames: &[ActionFrame], tick_rate_hz: u32) -> (PovCounts, PovTrace) {
    let mut counts = PovCounts::default();
    let mut held: [Vec<bool>; ALL_KEYS.len()] = Default::default();
    let mut trace = PovTrace {
        tuples: Vec::with_capacity(frames.len()),
        idle: Vec::with_capacity(frames.len()),
    };
    let mut positions: Vec<(f64, f64, f64)> = Vec::with_capacity(frames.len());
    let mut moving_flags: Vec<bool> = Vec::with_capacity(frames.len());
    let mut working_flags: Vec<bool> = Vec::with_capacity(frames.len());
    // The heading the turn started from, and the position it started at: `d_yaw` is what moved the
    // previous row's `yaw` to this row's, and the position pair is this row's travel.
    let mut previous: Option<(f64, f64, f64)> = None;

    for frame in frames {
        let bits = key_bits(&frame.keys);
        for (slot, &value) in held.iter_mut().zip(bits.iter()) {
            slot.push(value);
        }
        let discrete = [frame.attack, frame.use_item, frame.mine, frame.place_block];
        let camera = frame.d_yaw.abs() + frame.d_pitch.abs() > 1e-9;
        let moving = bits[..MOVE_KEY_COUNT].iter().any(|&b| b);

        counts.frames += 1;
        counts.moving += u64::from(moving);
        positions.push((frame.x, frame.y, frame.z));
        moving_flags.push(moving);
        working_flags.push(discrete.iter().any(|&b| b));
        trace
            .idle
            .push(!bits.iter().any(|&b| b) && !discrete.iter().any(|&b| b) && !camera);
        trace.tuples.push(pack(&bits, &discrete));

        // Camera/travel independence. A frame is evidence only when the bot was travelling fast
        // enough for "the direction of travel" to exist, the camera actually turned, and it was far
        // enough off that direction for "toward" to name something. The velocity is the horizontal
        // position difference from the previous row.
        if let Some((previous_yaw, previous_x, previous_z)) = previous {
            let (vx, vz) = (frame.x - previous_x, frame.z - previous_z);
            if frame.d_yaw.abs() > 1e-9 && vx.hypot(vz) >= CAMERA_SPEED_FLOOR {
                let error = wrap(travel_bearing(vx, vz) - previous_yaw);
                if error.abs() >= CAMERA_ANGLE_EPS {
                    counts.camera_frames += 1;
                    counts.camera_toward_travel += u64::from((frame.d_yaw > 0.0) == (error > 0.0));
                }
            }
        }
        previous = Some((frame.yaw, frame.x, frame.z));
    }

    counts.idle = trace.idle.iter().filter(|&&flag| flag).count() as u64;
    counts.longest_idle = runs(&trace.idle)
        .iter()
        .map(|&(_, length)| length as u64)
        .max()
        .unwrap_or(0);

    // Displacement-based stillness (see the FROZEN_DISPLACEMENT_BLOCKS block comment). `floor_y`
    // is the minimum y over the trailing window: the ground track under a jumping bot, the rising
    // staircase under a climbing one, so the vertical displacement term keeps climbing and drops
    // in-place jumping. The threshold pro-rates with the window while the stream is younger than
    // a second, making it a speed test rather than a free frozen ramp-in.
    let window = tick_rate_hz.max(1) as usize;
    let floor_y: Vec<f64> = (0..positions.len())
        .map(|i| {
            positions[i.saturating_sub(window)..=i]
                .iter()
                .map(|p| p.1)
                .fold(f64::INFINITY, f64::min)
        })
        .collect();
    let frozen: Vec<bool> = (0..positions.len())
        .map(|i| {
            let w = i.min(window);
            if w == 0 || working_flags[i] {
                return false;
            }
            let (x0, _, z0) = positions[i - w];
            let (x, _, z) = positions[i];
            let dy = floor_y[i] - floor_y[i - w];
            let travelled = ((x - x0).powi(2) + (z - z0).powi(2) + dy * dy).sqrt();
            travelled < FROZEN_DISPLACEMENT_BLOCKS * w as f64 / window as f64
        })
        .collect();
    counts.frozen = frozen.iter().filter(|&&flag| flag).count() as u64;
    counts.longest_frozen = runs(&frozen)
        .iter()
        .map(|&(_, length)| length as u64)
        .max()
        .unwrap_or(0);
    let stuck_min = (STUCK_MIN_RUN_S * f64::from(tick_rate_hz.max(1))) as usize;
    let pushing: Vec<bool> = frozen
        .iter()
        .zip(moving_flags.iter())
        .map(|(&frozen, &moving)| frozen && moving)
        .collect();
    counts.stuck = runs(&pushing)
        .iter()
        .filter(|&&(_, length)| length >= stuck_min)
        .map(|&(_, length)| length as u64)
        .sum();

    for movement in &held[..MOVE_KEY_COUNT] {
        for (_, length) in runs(movement) {
            counts.move_key_runs += 1;
            counts.move_key_runs_short += u64::from(length < SHORT_RUN_FRAMES);
        }
    }

    // Sprint identifiability: only presses long enough to have taken effect are evidence. A
    // micro-burst of forward is walk-without-sprint by construction (sprint needs sustained forward
    // motion to engage), so counting those would manufacture a contrast that is not a choice.
    let sustained = tick_rate_hz.max(1) as usize;
    let sprint = &held[ALL_KEYS.len() - 1];
    for (start, length) in runs(&held[0]) {
        if length >= sustained {
            counts.forward_runs_sustained += 1;
            counts.forward_runs_sustained_sprint +=
                u64::from(sprint[start..start + length].iter().any(|&b| b));
        }
    }

    counts.keys_never_pressed = ALL_KEYS
        .iter()
        .zip(held.iter())
        .filter(|(_, pressed)| !pressed.iter().any(|&b| b))
        .map(|(name, _)| (*name).to_string())
        .collect();

    (counts, trace)
}

/// Pack the seven key bits and the four discrete-action bits into one comparable word, so comparing
/// two frames' action tuples is a single integer equality.
fn pack(bits: &[bool; ALL_KEYS.len()], discrete: &[bool; 4]) -> u16 {
    let mut word = 0u16;
    for (index, &flag) in bits.iter().chain(discrete.iter()).enumerate() {
        word |= u16::from(flag) << index;
    }
    word
}

/// Count every POV's action stream and report threshold violations.
///
/// POVs are keyed by name (`p00`, ...); the `BTreeMap` gives a sorted iteration order, which is the
/// order failures are reported in.
pub fn check_episode(
    povs: &BTreeMap<String, Vec<ActionFrame>>,
    thresholds: &Thresholds,
    tick_rate_hz: u32,
) -> EpisodeQcOutcome {
    let mut outcome = EpisodeQcOutcome::default();
    if povs.is_empty() {
        outcome.failures.push("no action streams found".to_string());
        return outcome;
    }

    let mut traces: Vec<PovTrace> = Vec::new();
    let mut dead_keys: Option<BTreeSet<String>> = None;
    let mut pooled_toward = 0u64;
    let mut pooled_camera_frames = 0u64;

    for (pov, frames) in povs {
        let (counts, trace) = read_pov(frames, tick_rate_hz);
        traces.push(trace);
        dead_keys = Some(match dead_keys {
            None => counts.keys_never_pressed.clone(),
            Some(shared) => shared
                .intersection(&counts.keys_never_pressed)
                .cloned()
                .collect(),
        });
        pooled_toward += counts.camera_toward_travel;
        pooled_camera_frames += counts.camera_frames;

        let mut failures = Vec::new();
        if counts.frames == 0 {
            failures.push(format!("{pov}: empty action stream"));
        } else {
            if counts.moving < thresholds.min_moving_frames_per_pov {
                // The gap_jump p05 signature: one bot of eight simply did not take part.
                failures.push(format!(
                    "{pov}: only {} moving frames (min {}) — this bot barely participated",
                    counts.moving, thresholds.min_moving_frames_per_pov
                ));
            }
            if counts.idle_frac() > thresholds.max_idle_frac {
                failures.push(format!(
                    "{pov}: idle {} of {} frames (max {})",
                    pct(counts.idle_frac()),
                    counts.frames,
                    pct0(thresholds.max_idle_frac)
                ));
            }
            if counts.longest_idle_s(tick_rate_hz) > thresholds.max_idle_run_s {
                failures.push(format!(
                    "{pov}: idle (no keys pressed) for {:.1}s in one stretch (max {:.0}s)",
                    counts.longest_idle_s(tick_rate_hz),
                    thresholds.max_idle_run_s
                ));
            }
            if counts.move_key_runs >= MIN_MOVE_KEY_RUNS
                && counts.short_run_frac() > thresholds.max_short_run_frac
            {
                failures.push(format!(
                    "{pov}: {} of movement-key presses are shorter than {:.1}s (max {}) — key chatter",
                    pct(counts.short_run_frac()),
                    SHORT_RUN_FRAMES as f64 / f64::from(tick_rate_hz.max(1)),
                    pct0(thresholds.max_short_run_frac)
                ));
            }
            if counts.longest_frozen_s(tick_rate_hz) > thresholds.max_frozen_run_s {
                failures.push(format!(
                    "{pov}: no net displacement for {:.1}s in one stretch (max {:.0}s) — keys \
                     held or not, the body never moved",
                    counts.longest_frozen_s(tick_rate_hz),
                    thresholds.max_frozen_run_s
                ));
            }
            if counts.stuck_total_s(tick_rate_hz) > thresholds.max_stuck_total_s {
                failures.push(format!(
                    "{pov}: stuck for {:.1}s total (max {:.0}s) — movement keys held with no \
                     displacement for {STUCK_MIN_RUN_S:.0}s+ at a time, a bot shoving an obstacle",
                    counts.stuck_total_s(tick_rate_hz),
                    thresholds.max_stuck_total_s
                ));
            }
            if counts.forward_runs_sustained >= MIN_SUSTAINED_FORWARD_RUNS {
                let sprint_agreement = agreement(
                    counts.forward_runs_sustained_sprint,
                    counts.forward_runs_sustained,
                );
                if sprint_agreement > thresholds.max_sustained_sprint_agreement {
                    failures.push(format!(
                        "{pov}: {} of sustained forward runs agree on sprint (max {}) — the sprint \
                         bit carries no information",
                        pct(sprint_agreement),
                        pct0(thresholds.max_sustained_sprint_agreement)
                    ));
                }
            }
            if counts.camera_frames >= MIN_CAMERA_FRAMES {
                let camera_agreement = counts.camera_travel_agreement();
                if camera_agreement > thresholds.max_camera_travel_agreement {
                    let toward = ratio(counts.camera_toward_travel, counts.camera_frames);
                    let heading = if toward >= 0.5 { "toward" } else { "away from" };
                    failures.push(format!(
                        "{pov}: {} of camera turns while moving went {heading} the direction of \
                         travel (max {}) — the camera restates the path instead of pointing \
                         anywhere the path does not",
                        pct(camera_agreement),
                        pct0(thresholds.max_camera_travel_agreement)
                    ));
                }
            }
        }

        outcome.failures.extend(failures.iter().cloned());
        outcome.povs.insert(
            pov.clone(),
            PovQcOutcome {
                metrics: DegeneracyMetrics {
                    frames: counts.frames,
                    idle_frac: round_to(counts.idle_frac(), 4),
                    longest_idle_s: round_to(counts.longest_idle_s(tick_rate_hz), 2),
                    moving: counts.moving,
                    short_run_frac: round_to(counts.short_run_frac(), 4),
                    forward_runs_sustained: counts.forward_runs_sustained,
                    forward_runs_sustained_sprint: counts.forward_runs_sustained_sprint,
                    camera_frames: counts.camera_frames,
                    camera_travel_agreement: round_to(counts.camera_travel_agreement(), 4),
                    frozen_frac: round_to(counts.frozen_frac(), 4),
                    longest_frozen_s: round_to(counts.longest_frozen_s(tick_rate_hz), 2),
                    stuck_total_s: round_to(counts.stuck_total_s(tick_rate_hz), 2),
                    // Video-level; filled in by the finalize stage, which has the video.
                    dark_frame_frac: None,
                    longest_dark_run_s: None,
                },
                failures,
            },
        );
    }

    outcome.camera_travel_agreement = round_to(agreement(pooled_toward, pooled_camera_frames), 4);
    outcome.dead_keys = dead_keys.unwrap_or_default().into_iter().collect();

    let missing: Vec<&String> = thresholds
        .require_keys
        .iter()
        .filter(|name| outcome.dead_keys.contains(name))
        .collect();
    if !missing.is_empty() {
        let names: Vec<&str> = missing.iter().map(|name| name.as_str()).collect();
        outcome.failures.push(format!(
            "never pressed by any bot: {} — a conditioning bit that is constant in training is \
             inert at inference",
            names.join(", ")
        ));
    }

    let mut agreements: Vec<f64> = Vec::new();
    for i in 0..traces.len() {
        for j in (i + 1)..traces.len() {
            let (left, right) = (&traces[i], &traces[j]);
            let n = left.tuples.len().min(right.tuples.len());
            let active: Vec<usize> = (0..n)
                .filter(|&k| !(left.idle[k] && right.idle[k]))
                .collect();
            if active.is_empty() {
                continue;
            }
            let same = active
                .iter()
                .filter(|&&k| left.tuples[k] == right.tuples[k])
                .count();
            agreements.push(same as f64 / active.len() as f64);
        }
    }
    if !agreements.is_empty() {
        let mean = agreements.iter().sum::<f64>() / agreements.len() as f64;
        outcome.pair_agreement_active = round_to(mean, 4);
        if mean > thresholds.max_pair_agreement_active {
            outcome.failures.push(format!(
                "players agree on {} of active frames (max {}) — the bots are near-clones",
                pct(mean),
                pct0(thresholds.max_pair_agreement_active)
            ));
        }
    }

    outcome
}

/// Judge one POV's video brightness from sampled per-frame mean luma (`signalstats` YAVG, 0-255).
///
/// Pure over samples the caller extracted — the ffmpeg invocation lives with the other ffmpeg
/// calls in the render crate, and the sampling rate is the caller's choice (finalize samples the
/// default projection at 2 frames/s: darkness QC asks about multi-second stretches, which that
/// resolves at a fraction of a full decode). `samples_per_s` converts run lengths back to seconds.
///
/// Returns `(dark_frame_frac, longest_dark_run_s, failures)`, rounded the way every other stored
/// metric is; failure messages are formatted from the unrounded values.
pub fn check_pov_darkness(
    pov: &str,
    luma_yavg: &[f64],
    samples_per_s: f64,
    thresholds: &Thresholds,
) -> (f64, f64, Vec<String>) {
    let dark: Vec<bool> = luma_yavg.iter().map(|&y| y < DARK_LUMA_MAX).collect();
    let frac = ratio(
        dark.iter().filter(|&&flag| flag).count() as u64,
        dark.len() as u64,
    );
    let longest_run_s = runs(&dark)
        .iter()
        .map(|&(_, length)| length)
        .max()
        .unwrap_or(0) as f64
        / samples_per_s.max(f64::MIN_POSITIVE);

    let mut failures = Vec::new();
    if frac > thresholds.max_dark_frame_frac {
        failures.push(format!(
            "{pov}: {} of sampled video frames are near-black (mean luma < {DARK_LUMA_MAX:.0}/255, \
             max {}) — the camera spent most of the episode in the dark",
            pct(frac),
            pct0(thresholds.max_dark_frame_frac)
        ));
    }
    if longest_run_s > thresholds.max_dark_run_s {
        failures.push(format!(
            "{pov}: near-black video for {longest_run_s:.1}s in one stretch (max {:.0}s) — a bot \
             that settled into unlit darkness",
            thresholds.max_dark_run_s
        ));
    }
    (round_to(frac, 4), round_to(longest_run_s, 2), failures)
}

/// Cross-POV time-base guard, judged at finalize on the post-trim rows. Returns
/// `(spread, failures)` where spread = max − min over the shipped files' first-row
/// `worldAgeTicks`.
///
/// The finalize trim converts one common world-tick span to each POV's own frame window, so
/// every shipped file's frame 0 sits on the same world tick and the spread is 0 by
/// construction. A nonzero spread means the trim regressed to something frame-index-shaped:
/// sharded captures start their recordings ~14 ticks apart (a 16-POV episode measured 51),
/// and a frame-index window ships files whose equal frame numbers and equal wall clocks sit
/// on different world moments.
pub fn check_pov_anchor_spread(
    first_ticks: &BTreeMap<String, i64>,
    thresholds: &Thresholds,
) -> (i64, Vec<String>) {
    let (Some(min), Some(max)) = (first_ticks.values().min(), first_ticks.values().max()) else {
        return (0, Vec::new());
    };
    let spread = max - min;
    let mut failures = Vec::new();
    if spread > thresholds.max_pov_anchor_spread_ticks {
        failures.push(format!(
            "episode: shipped POV windows disagree by {spread} ticks (max {}) — rows with equal \
             frame numbers and wall-clock times sit on different worldAgeTicks across POVs",
            thresholds.max_pov_anchor_spread_ticks
        ));
    }
    (spread, failures)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pawa_schema::time::{ClientTick, PlayerTimeMs, ServerTick};

    /// One frame: everything not named here is the neutral value, so a test row states only what it
    /// is testing.
    fn frame(tick: i64, forward: bool, d_yaw: f64) -> ActionFrame {
        ActionFrame {
            client_tick: ClientTick(tick),
            server_tick: ServerTick(tick),
            world_age_tick: None,
            player_time_ms: PlayerTimeMs(tick * 50),
            x: 0.0,
            y: 64.0,
            z: 0.0,
            yaw: 0.0,
            pitch: 0.0,
            d_yaw,
            d_pitch: 0.0,
            keys: MovementKeys {
                forward,
                back: false,
                left: false,
                right: false,
                jump: false,
                sprint: false,
                sneak: false,
            },
            attack: false,
            use_item: false,
            place_block: false,
            mine: false,
            hotbar_slot: None,
            held_item: None,
            is_in_water: false,
            inventory: None,
        }
    }

    #[test]
    fn wall_shove_is_stuck_and_jumping_in_place_does_not_clear_it() {
        // 600 frames (30 s at 20 Hz) of holding W into a wall while bunny-hopping in place: x and
        // z never change, y oscillates a jump arc. Key-based idle sees a busy bot; displacement
        // must see a statue.
        let stream: Vec<ActionFrame> = (0..600)
            .map(|tick| {
                let mut row = frame(tick, true, 0.0);
                row.y = 64.0 + if tick % 12 < 6 { 0.0 } else { 1.25 };
                row
            })
            .collect();
        let povs: BTreeMap<String, Vec<ActionFrame>> =
            [("p00".to_string(), stream)].into_iter().collect();
        let outcome = check_episode(&povs, &Thresholds::default(), 20);
        let metrics = &outcome.povs["p00"].metrics;

        // Frame 0 has no window yet; everything after is frozen despite the held key and the jumps.
        assert_eq!(metrics.longest_frozen_s, 29.95);
        assert_eq!(metrics.stuck_total_s, 29.95);
        assert!(
            outcome.failures.iter().any(|f| f.contains("stuck for"))
                && outcome
                    .failures
                    .iter()
                    .any(|f| f.contains("no net displacement")),
            "expected frozen and stuck failures, got {:?}",
            outcome.failures
        );
    }

    #[test]
    fn stationary_building_is_working_not_stuck() {
        // A builder standing in place for 30 s, laying a block every 2 s, no movement keys: the
        // placement frames break the frozen runs and no stuck time accrues.
        let stream: Vec<ActionFrame> = (0..600)
            .map(|tick| {
                let mut row = frame(tick, false, 0.0);
                row.place_block = tick % 40 == 0;
                row
            })
            .collect();
        let povs: BTreeMap<String, Vec<ActionFrame>> =
            [("p00".to_string(), stream)].into_iter().collect();
        let outcome = check_episode(&povs, &Thresholds::default(), 20);
        let metrics = &outcome.povs["p00"].metrics;

        assert_eq!(metrics.stuck_total_s, 0.0);
        assert!(
            metrics.longest_frozen_s < 3.0,
            "placement should break frozen runs, got {}",
            metrics.longest_frozen_s
        );

        // The same stream without the placements is the statue the metric exists for.
        let statue: Vec<ActionFrame> = (0..600).map(|tick| frame(tick, false, 0.0)).collect();
        let povs: BTreeMap<String, Vec<ActionFrame>> =
            [("p00".to_string(), statue)].into_iter().collect();
        let outcome = check_episode(&povs, &Thresholds::default(), 20);
        assert!(
            outcome
                .failures
                .iter()
                .any(|f| f.contains("no net displacement")),
            "a 30s statue must fail the frozen bound, got {:?}",
            outcome.failures
        );
    }

    #[test]
    fn climbing_a_pillar_counts_as_displacement() {
        // Rising 2 blocks/s straight up: the rolling floor climbs with the bot, so after the
        // window ramps in nothing is frozen.
        let stream: Vec<ActionFrame> = (0..600)
            .map(|tick| {
                let mut row = frame(tick, false, 0.0);
                row.y = 64.0 + 0.1 * tick as f64;
                row
            })
            .collect();
        let povs: BTreeMap<String, Vec<ActionFrame>> =
            [("p00".to_string(), stream)].into_iter().collect();
        let outcome = check_episode(&povs, &Thresholds::default(), 20);
        let metrics = &outcome.povs["p00"].metrics;

        assert_eq!(metrics.stuck_total_s, 0.0);
        assert!(
            metrics.longest_frozen_s <= 2.0,
            "only the window ramp-in may read as frozen, got {}",
            metrics.longest_frozen_s
        );
    }

    #[test]
    fn scenario_tiers_relax_keys_but_never_displacement() {
        let build = Thresholds::for_scenario("build_house");
        let default = Thresholds::default();
        assert!(build.max_idle_frac > default.max_idle_frac);
        assert!(build.max_idle_run_s > default.max_idle_run_s);
        assert!(build.min_moving_frames_per_pov < default.min_moving_frames_per_pov);
        assert_eq!(build.max_frozen_run_s, default.max_frozen_run_s);
        assert_eq!(build.max_stuck_total_s, default.max_stuck_total_s);

        let chase = Thresholds::for_scenario("no_contact_pair_chase");
        assert_eq!(chase.max_camera_travel_agreement, 1.0);
        assert_eq!(chase.max_idle_frac, default.max_idle_frac);

        let combat = Thresholds::for_scenario("pvp");
        assert!(combat.max_short_run_frac > default.max_short_run_frac);
        assert_eq!(combat.max_frozen_run_s, default.max_frozen_run_s);

        assert_eq!(Thresholds::for_scenario("not_a_scenario"), default);
    }

    #[test]
    fn two_identical_povs_score_as_clones() {
        // 40 frames: the first 30 walk forward, the last 10 do nothing at all.
        let stream: Vec<ActionFrame> = (0..40).map(|tick| frame(tick, tick < 30, 0.0)).collect();
        let povs: BTreeMap<String, Vec<ActionFrame>> = [
            ("p00".to_string(), stream.clone()),
            ("p01".to_string(), stream),
        ]
        .into_iter()
        .collect();

        let outcome = check_episode(&povs, &Thresholds::default(), 20);
        let p00 = &outcome.povs["p00"].metrics;

        assert_eq!(p00.frames, 40);
        assert_eq!(p00.moving, 30);
        assert_eq!(p00.idle_frac, 0.25);
        assert_eq!(p00.longest_idle_s, 0.5);
        // One 30-tick forward run: not short, and sustained past the one-second bar without sprint.
        assert_eq!(p00.short_run_frac, 0.0);
        assert_eq!(p00.forward_runs_sustained, 1);
        assert_eq!(p00.forward_runs_sustained_sprint, 0);
        // No position ever changes, so no frame is camera/travel evidence.
        assert_eq!(p00.camera_frames, 0);

        // Every key but `forward` is untouched by both POVs, so all six are dead episode-wide.
        assert_eq!(
            outcome.dead_keys,
            vec!["back", "jump", "left", "right", "sneak", "sprint"]
        );
        // Identical streams: total agreement, which is exactly the clone finding.
        assert_eq!(outcome.pair_agreement_active, 1.0);
        assert!(
            outcome
                .failures
                .iter()
                .any(|failure| failure.contains("near-clones")),
            "expected a clone failure, got {:?}",
            outcome.failures
        );
        // 30 moving frames is far under the 200-frame participation floor, for both POVs.
        assert_eq!(
            outcome
                .failures
                .iter()
                .filter(|failure| failure.contains("barely participated"))
                .count(),
            2
        );

        // A bot walking due +x at 0.3 blocks/tick while turning its camera the same way every
        // tick. Travel bearing is a constant -π/2 against a yaw of 0, so every turn but the first
        // frame's is evidence and every one of them disagrees — which scores 1.0, because the
        // metric measures predictability, not direction.
        let walker: Vec<ActionFrame> = (0..40)
            .map(|tick| {
                let mut row = frame(tick, true, 0.05);
                row.x = 0.3 * tick as f64;
                row
            })
            .collect();
        let walking: BTreeMap<String, Vec<ActionFrame>> =
            [("p00".to_string(), walker)].into_iter().collect();
        let outcome = check_episode(&walking, &Thresholds::default(), 20);

        assert_eq!(outcome.povs["p00"].metrics.camera_frames, 39);
        assert_eq!(outcome.povs["p00"].metrics.camera_travel_agreement, 1.0);
        assert_eq!(outcome.camera_travel_agreement, 1.0);
        // Under MIN_CAMERA_FRAMES, so predictability is measured but not yet held against it.
        assert!(
            !outcome
                .failures
                .iter()
                .any(|failure| failure.contains("restates the path")),
            "39 frames is too little evidence to fail on, got {:?}",
            outcome.failures
        );
    }

    #[test]
    fn anchor_spread_gate() {
        let thresholds = Thresholds::default();
        let deltas = |values: &[(&str, i64)]| -> BTreeMap<String, i64> {
            values
                .iter()
                .map(|&(pov, delta)| (pov.to_string(), delta))
                .collect()
        };

        // No aligned POVs and a single POV: spread 0, no failure.
        assert_eq!(
            check_pov_anchor_spread(&deltas(&[]), &thresholds),
            (0, vec![])
        );
        assert_eq!(
            check_pov_anchor_spread(&deltas(&[("p00", 100)]), &thresholds),
            (0, vec![])
        );
        // At the bound (2 ticks): passes.
        let (spread, failures) =
            check_pov_anchor_spread(&deltas(&[("p00", 100), ("p01", 102)]), &thresholds);
        assert_eq!((spread, failures.len()), (2, 0));
        // The 16p case: 51 ticks of spread must fail.
        let (spread, failures) = check_pov_anchor_spread(
            &deltas(&[("p00", 100), ("p07", 130), ("p15", 151)]),
            &thresholds,
        );
        assert_eq!(spread, 51);
        assert!(
            failures.iter().any(|f| f.contains("51 ticks")),
            "expected a spread failure, got {failures:?}"
        );
    }
}

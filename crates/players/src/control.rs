//! Decoupled, quantized control plane.
//!
//! Replaces the old "pathfinder steers the head + wall-clock-interpolated `look`" execution
//! (which produced violent single-tick camera snaps) with TWO independent, fixed-rate channels:
//!
//!   * camera  — yaw/pitch each advance by at most one fixed angular quantum per tick, toward a
//!               scenario-set target (look-at / sweep / hold). Quantum = rate_deg_s / 20.
//!   * motion  — a discrete 8-way [`WalkDirection`] chosen *body-relative to the current camera
//!               yaw* so the bot walks toward its world waypoint while the camera looks wherever
//!               it likes. The raw per-tick request is smoothed by [`latch_step`]: an
//!               identity-dwell (direction changes — direct or through a brief stop — are rate
//!               limited to one per `min_dwell_ticks`), a transient-stop debounce (a `None` request
//!               mid-dwell is absorbed while the waypoint is still further than
//!               `REVERSAL_ARRIVE_DIST`, so a one-tick stop can no longer end a key press), plus an
//!               overshoot settle (a >90° request flip next to the waypoint means we walked past it
//!               → stand still briefly instead of chasing it backward). Sprint is an INDEPENDENT
//!               per-bout draw ([`SprintBout`]) and then disengages on final approach
//!               (`SPRINT_APPROACH_DIST`); it is deliberately not a function of the navigation
//!               state, or the recorded bit would be inferable from position and teach the model
//!               nothing. The controller owns ALL jumps (step-up / pre-contact ascent / water).
//!               Once a bot's script is finished it wanders a short leash around where it stopped
//!               ([`ambient_target`]) instead of standing still for the rest of the recording.
//!
//! Why ECS systems and not the async `Event::Tick` handler: the camera/motion writes must land
//! *before* `PhysicsSystems` (so `tick_controls`/`ai_step`/`send_*` consume them this tick) and the
//! capture must read *after* `PhysicsSystems` (post-physics pose = what the replay/video frame shows).
//! An async handler has no ordering guarantee against azalea's GameTick schedule. Navigation is
//! planner-owned (see nav.rs): azalea's `goto`/executor are never engaged — a per-bot driver
//! task calls azalea's public `calculate_path` and the controller follows OUR corridor, so path
//! progression, replanning, and giving up are decided in one place.

// The doc bullets above hang-indent their continuations for readability.
#![allow(clippy::doc_overindented_list_items)]

use std::sync::atomic::Ordering;

use azalea::{
    app::{App, Plugin},
    core::tick::GameTick,
    ecs::prelude::*,
    inventory::{ItemStack, components::MaxStackSize, default_components::DefaultableComponent},
    mining::Mining,
    physics::PhysicsSystems,
    physics::client_movement::{ClientMovementState, WalkDirection},
};
use azalea_entity::{Jumping, LookDirection, Physics, Position, inventory::Inventory};
use pawa_schema::capture::ActionFrame;
use pawa_schema::dataset::{InventorySnapshot, ItemSnapshot, MovementKeys};
use pawa_schema::time::{ClientTick, PlayerTimeMs, ServerTick, WorldAgeTick};

use crate::{BotState, Shared, item_name, plan::Mulberry32, r3, shortest_arc, shortest_arc_deg};

// ---------------------------------------------------------------------------------------------
// Calibration constants. Angular rates are also overridable via Cfg (yaw_rate_deg_s/pitch_rate_deg_s); these are
// the per-axis defaults expressed as the v7 spec: yaw 360 deg / 5 s, pitch 90 deg / 2 s.
// ---------------------------------------------------------------------------------------------

pub const DEFAULT_YAW_RATE_DEG_S: f64 = 72.0; // 360 / 5
pub const DEFAULT_PITCH_RATE_DEG_S: f64 = 45.0; // 90 / 2
const TICKS_PER_SEC: f64 = 20.0;

/// Strafe handedness calibration. On azalea 0.16 / MC 26.2, `WalkDirection::{Left,Right}` moves
/// the player toward its VISUAL left/right — no mirror. An earlier azalea DID mirror the strafe,
/// and the `-1.0` correction that compensated for it survived the upgrade, quietly re-introducing
/// the mirror: a calibration episode measured every left/right displacement flipped
/// by exactly the strafe sign (forward/back exact) while forward-only segments were perfect.
/// Recalibrate by running the `calibration` scenario end-to-end and checking each single-key
/// segment's displacement bearing against its recorded keys (calibration-analysis style:
/// W=0°, D=+90°, A=-90°, S=180°).
const STRAFE_SIGN: f64 = 1.0;

/// Stop sprinting when the current waypoint is closer than this (blocks): approach at walk speed
/// so per-tick displacement and post-stop slide stay under the node eps, and azalea's FINAL-node
/// pop (centered ±0.2 with near-zero velocity, on ground) is actually satisfiable.
const SPRINT_APPROACH_DIST: f64 = 3.0;

/// Overshoot hysteresis: a requested direction more than 90° away from the direction we were
/// just traveling, while the waypoint is within this range, means we walked PAST the waypoint —
/// settle (stand still) instead of chasing it backward. Chasing is what produced the
/// forward(5)→back(5)→forward(5) limit cycle measured in v1 data (61% of non-idle runs exactly
/// one dwell long, 396 adjacent reversals in 20 sampled POVs).
const REVERSAL_ARRIVE_DIST: f64 = 2.0;

/// Ticks to stand still after an overshoot before re-approaching. At least the default dwell,
/// so by the time the settle expires the dwell clock has also expired and the (gentle, no-sprint)
/// re-approach direction is never itself suppressed.
const SETTLE_TICKS: u32 = 12;

// ---------------------------------------------------------------------------------------------
// Pure helpers (unit-tested; no azalea/ECS state).
// ---------------------------------------------------------------------------------------------

/// One fixed-rate quantized rotation step toward a target, in degrees.
///
/// `rem` is the shortest-arc remaining angle (target − current). Returns `(applied, label)`:
///   * `|rem| > q/2`  → applied = clamp(rem, ±q), label = sign(applied)  (a real action)
///   * `|rem| ≤ q/2`  → applied = 0, label = 0                            (no-op; settle)
///
/// Invariant (relied on by the recorder): `applied == 0` ⟺ `label == 0`, and a non-zero applied
/// magnitude is always in `(q/2, q]`. So `d_angle ∈ {0} ∪ (q/2, q]` and the discrete label never
/// disagrees with the recorded delta.
pub fn quantize_step(rem_deg: f64, q_deg: f64) -> (f64, i8) {
    let half = q_deg * 0.5;
    if rem_deg.abs() > half {
        let applied = rem_deg.clamp(-q_deg, q_deg);
        (applied, if applied >= 0.0 { 1 } else { -1 })
    } else {
        (0.0, 0)
    }
}

/// Yaw/pitch (degrees) that makes an eye at `eye` look at `target`. Mirrors azalea's
/// `direction_looking_at` so our planned look matches engine geometry; pitch clamped to ±90.
pub fn look_angles(eye: (f64, f64, f64), target: (f64, f64, f64)) -> (f64, f64) {
    let dx = target.0 - eye.0;
    let dy = target.1 - eye.1;
    let dz = target.2 - eye.2;
    let ground = (dx * dx + dz * dz).sqrt();
    let yaw = (std::f64::consts::PI - (-dx).atan2(-dz)).to_degrees();
    let pitch = (dy.atan2(ground) * -(180.0 / std::f64::consts::PI)).clamp(-90.0, 90.0);
    (yaw, pitch)
}

/// Sector-switch hysteresis (degrees past the 22.5° boundary) for [`body_relative_dir`]: a target
/// bearing drifting across a sector boundary dithers the requested 8-way every few ticks (the
/// residual 45° F↔FL churn after the reversal fixes — 45.8% of presses under 0.5 s in v4 data).
/// Keep the HELD direction until the bearing is this far INSIDE the neighboring sector, so only
/// genuine turns switch keys; a fresh start (no held direction) snaps to the nearest sector.
const SECTOR_HYST_DEG: f64 = 10.0;

/// Bearing of world delta `(dx, dz)` in the body frame at camera `yaw_deg`: 0° = straight
/// forward, +90° = to the (STRAFE_SIGN-corrected) right, range (-180, 180].
fn body_bearing_deg(dx: f64, dz: f64, yaw_deg: f64) -> f64 {
    let yaw = yaw_deg.to_radians();
    let (s, c) = (yaw.sin(), yaw.cos());
    // forward_world = (-s, c); right_world = (-c, -s) (player's right at yaw 0 / south is west).
    let fwd = dx * (-s) + dz * c;
    let right = STRAFE_SIGN * (dx * (-c) + dz * (-s));
    right.atan2(fwd).to_degrees()
}

fn sector_to_dir(sector: i64) -> WalkDirection {
    match sector.rem_euclid(8) {
        0 => WalkDirection::Forward,
        1 => WalkDirection::ForwardRight,
        2 => WalkDirection::Right,
        3 => WalkDirection::BackwardRight,
        4 => WalkDirection::Backward,
        5 => WalkDirection::BackwardLeft,
        6 => WalkDirection::Left,
        _ => WalkDirection::ForwardLeft,
    }
}

/// 8-way body-relative direction toward world delta `(dx, dz)` given the current camera `yaw_deg`,
/// or `None` if the delta is within `eps` (arrived → stand still).
///
/// MC convention: forward (W) world vector at yaw θ is `(-sinθ, cosθ)`; the player's right is
/// perpendicular. We resolve the delta into (forward, right) components and pick the nearest of 8
/// sectors (45° each). Forward/back is convention-robust; left/right carries [`STRAFE_SIGN`].
/// `held` (the currently APPLIED direction, `None` when stopped) adds the [`SECTOR_HYST_DEG`]
/// Schmitt band: the held sector is kept until the bearing is well inside a neighbor.
pub fn body_relative_dir(
    dx: f64,
    dz: f64,
    yaw_deg: f64,
    eps: f64,
    held: WalkDirection,
) -> Option<WalkDirection> {
    if (dx * dx + dz * dz).sqrt() <= eps {
        return None;
    }
    let ang = body_bearing_deg(dx, dz, yaw_deg);
    if let Some(c) = dir_sector(held) {
        let delta = shortest_arc_deg(ang - 45.0 * c as f64).abs();
        if delta <= 22.5 + SECTOR_HYST_DEG {
            return Some(held);
        }
    }
    Some(sector_to_dir(((ang / 45.0).round() as i64) % 8 + 8)) // 0=F,1=FR,…,7=FL
}

/// Latch + overshoot-settle state for the APPLIED walk direction (one per bot; see
/// [`latch_step`]). The dwell clock follows the IDENTITY of the last non-stop direction and
/// keeps ticking through stops, so a brief `None` gap can no longer reset it.
#[derive(Clone, Copy, Debug)]
pub struct MoveLatch {
    /// Direction applied last tick.
    pub dir: WalkDirection,
    /// Last non-`None` applied direction (identity persists across stops).
    pub last_move: WalkDirection,
    /// Ticks since `last_move` last changed identity (keeps counting while stopped).
    pub since_change: u32,
    /// Remaining overshoot-settle ticks (apply `None` while > 0).
    pub settle: u32,
    /// Waypoint (x, z) the settle was last armed against — the settle fires at most once per
    /// waypoint, so an expired settle can never re-arm into a standstill loop.
    pub settle_wp: (f64, f64),
}

impl Default for MoveLatch {
    fn default() -> Self {
        MoveLatch {
            dir: WalkDirection::None,
            last_move: WalkDirection::None,
            since_change: u32::MAX, // a fresh bot may start moving immediately
            settle: 0,
            settle_wp: (f64::INFINITY, f64::INFINITY), // != any real waypoint
        }
    }
}

/// 8-way sector index of a direction in the azalea-name frame (F=0, FR=1, R=2, … FL=7).
fn dir_sector(d: WalkDirection) -> Option<i32> {
    Some(match d {
        WalkDirection::None => return None,
        WalkDirection::Forward => 0,
        WalkDirection::ForwardRight => 1,
        WalkDirection::Right => 2,
        WalkDirection::BackwardRight => 3,
        WalkDirection::Backward => 4,
        WalkDirection::BackwardLeft => 5,
        WalkDirection::Left => 6,
        WalkDirection::ForwardLeft => 7,
    })
}

/// Angular gap between two directions in 45° sectors (0..=4), `None` if either is `None`.
pub fn sector_gap(a: WalkDirection, b: WalkDirection) -> Option<i32> {
    let (a, b) = (dir_sector(a)?, dir_sector(b)?);
    let d = (a - b).rem_euclid(8);
    Some(d.min(8 - d))
}

/// Minimum-dwell decision: applying a non-stop direction whose IDENTITY differs from the last
/// applied non-stop direction requires that identity to have stood `min_dwell` ticks. Stops and
/// same-direction resumes are NEVER delayed — goal arrival / safety must not overshoot. Because
/// the clock follows identity (not the currently-held key), a 1-2-tick stop gap no longer lets a
/// direction flip bypass the dwell (measured in v1 data: 211 instant reversals through such
/// gaps in 20 POVs). `min_dwell == 0` disables the latch.
pub fn latch_decide(
    last_move: WalkDirection,
    since_change: u32,
    current: WalkDirection,
    requested: WalkDirection,
    min_dwell: u32,
) -> WalkDirection {
    if min_dwell == 0
        || matches!(requested, WalkDirection::None)
        || requested == last_move
        || since_change >= min_dwell
    {
        requested
    } else if matches!(current, WalkDirection::None) {
        // Mid-dwell start from a stop toward a NEW direction: stay stopped (re-starting the old
        // direction would walk somewhere we no longer want to go).
        WalkDirection::None
    } else {
        current
    }
}

/// One per-tick latch step: overshoot settle + minimum dwell, in that order. Pure (unit-tested);
/// `apply_intents` calls it with the freshly requested 8-way and the current waypoint.
///
/// Overshoot settle: if the request is >90° away from the direction we were just traveling while
/// the waypoint is `REVERSAL_ARRIVE_DIST` or closer, we just walked past the waypoint — stand
/// still for `SETTLE_TICKS` instead of chasing it backward (azalea usually pops the node during
/// the stop; if not, the post-settle re-approach walks back gently, sprint already disabled by
/// the approach gate). Armed at most once per waypoint (`settle_wp`), so it cannot re-trigger
/// into a permanent standstill.
pub fn latch_step(
    latch: &mut MoveLatch,
    requested: WalkDirection,
    waypoint: Option<(f64, f64)>,
    wp_dist: Option<f64>,
    goal_dist: Option<f64>,
    min_dwell: u32,
) -> WalkDirection {
    // A TRANSIENT stop is not a decision. Sector hysteresis and corridor re-planning can request
    // `None` for one or two ticks in the middle of a travel bout, and because the stop path used to
    // bypass the dwell entirely, that ended the key press. That is the measured defect: 27-49% of
    // movement-key presses in the shipped corpus lasted under 0.5 s while this dwell floor is 0.5 s,
    // and the 1-4-tick movement bouts moved the player a median of 0.43 blocks -- an action bit
    // flipped for nothing. Hold the current direction through such a stop.
    //
    // GATED ON THE FINAL GOAL, NOT THE NODE. The first version read `wp_dist` here and measured
    // WORSE than no gate at all (in a validation capture: short presses
    // rose to 44.7% on forward and 93.1% on left). Under Planned nav `wp_dist` is the distance to a
    // lookahead CORRIDOR node and `nav::ADVANCE_RADIUS` is 1.5, so it sits at or below
    // REVERSAL_ARRIVE_DIST essentially every tick — the gate was false almost always and the
    // debounce never ran in the nav mode that produces the chatter. `goal_dist` is the distance to
    // the episode's actual target, which is what "about to arrive" has to mean. The sprint gate
    // below already drew this distinction, with a comment explaining why; this one did not.
    //
    // An arrival at the real goal, an Idle/Hold intent (no waypoint at all), and min_dwell == 0 all
    // keep their immediate stop.
    let requested = if min_dwell > 0
        && matches!(requested, WalkDirection::None)
        && !matches!(latch.dir, WalkDirection::None)
        && latch.since_change < min_dwell
        && waypoint.is_some()
        && goal_dist.is_none_or(|d| d > REVERSAL_ARRIVE_DIST)
    {
        latch.dir
    } else {
        requested
    };
    let wp_is_new = |latch: &MoveLatch, wp: (f64, f64)| {
        (wp.0 - latch.settle_wp.0).abs() > 0.75 || (wp.1 - latch.settle_wp.1).abs() > 0.75
    };
    match waypoint {
        None => latch.settle = 0, // Idle/Hold or path consumed: nothing to settle against
        Some(wp) => {
            if min_dwell > 0
                && latch.settle == 0
                && wp_is_new(latch, wp)
                && wp_dist.is_some_and(|d| d <= REVERSAL_ARRIVE_DIST)
                && sector_gap(latch.last_move, requested).is_some_and(|g| g >= 3)
            {
                latch.settle = SETTLE_TICKS;
                latch.settle_wp = wp;
            }
        }
    }
    let applied = if latch.settle > 0 {
        latch.settle -= 1;
        WalkDirection::None
    } else {
        latch_decide(
            latch.last_move,
            latch.since_change,
            latch.dir,
            requested,
            min_dwell,
        )
    };
    if !matches!(applied, WalkDirection::None) && applied != latch.last_move {
        latch.last_move = applied;
        latch.since_change = 1;
    } else {
        latch.since_change = latch.since_change.saturating_add(1);
    }
    latch.dir = applied;
    applied
}

/// Per-bot sprint-bout state. A "bout" is one maximal run of continuous applied movement; the
/// sprint decision is drawn once when it starts and held until it ends.
#[derive(Default, Clone, Copy, Debug)]
pub struct SprintBout {
    pub in_bout: bool,
    pub sprint: bool,
    pub bouts: u64,
}

impl SprintBout {
    /// Advance the bout state for this tick and return whether this bout intends to sprint.
    ///
    /// The whole point is that the answer does NOT depend on position, distance, or direction:
    /// those are what made the recorded sprint bit a restatement of the navigation state instead
    /// of a control signal (2026-08 action audit, §1). It depends only on which bout this is.
    pub fn tick(&mut self, moving: bool, seed: u32, bot_index: usize, prob: f64) -> bool {
        if !moving {
            self.in_bout = false;
            self.sprint = false;
            return false;
        }
        if !self.in_bout {
            self.in_bout = true;
            self.bouts += 1;
            self.sprint = sprint_for_bout(seed, bot_index, self.bouts, prob);
        }
        self.sprint
    }
}

/// Deterministic per-bout sprint draw: same (seed, bot, bout) always decides the same way, so a
/// re-render of the same episode produces the same action stream. Uses the scenario RNG the rest of
/// the runtime uses (`plan::Mulberry32`), seeded the way `ScenarioEngine::bot_rng` seeds a bot and
/// then mixed with the bout counter.
pub fn sprint_for_bout(seed: u32, bot_index: usize, bout: u64, prob: f64) -> bool {
    if prob <= 0.0 {
        return false;
    }
    if prob >= 1.0 {
        return true;
    }
    let bot_mix = seed ^ ((bot_index as u32).wrapping_add(1)).wrapping_mul(0x9E37_79B9);
    let mixed = bot_mix ^ ((bout as u32).wrapping_add(1)).wrapping_mul(0x85EB_CA6B);
    Mulberry32::new(mixed).next() < prob
}

/// A post-finish ambient target: a point within `leash` blocks of where the bot finished, redrawn
/// every `switch_ticks`. Pure, so the wander is reproducible from the episode seed.
///
/// This exists because `apply_intents` used to return early for a finished bot, which is where the
/// long dead stretches in the shipped corpus come from (audit §2: single freezes up to 114.5 s,
/// mostly a bot that finished its script while peers were still running). Walking a small leash is
/// enough to keep the action stream and the video alive without letting a done bot drift into a
/// peer that is still working.
pub fn ambient_target(
    seed: u32,
    bot_index: usize,
    anchor: (f64, f64, f64),
    epoch: u64,
    leash: f64,
) -> (f64, f64) {
    let bot_mix = seed ^ ((bot_index as u32).wrapping_add(1)).wrapping_mul(0x9E37_79B9);
    let mut rng =
        Mulberry32::new(bot_mix ^ ((epoch as u32).wrapping_add(1)).wrapping_mul(0x27D4_EB2D));
    let angle = rng.next() * std::f64::consts::TAU;
    // sqrt keeps the draw uniform over the disc instead of clustering at the anchor.
    let radius = leash * rng.next().sqrt();
    (
        anchor.0 + radius * angle.cos(),
        anchor.2 + radius * angle.sin(),
    )
}

fn is_forwardish(dir: WalkDirection) -> bool {
    matches!(
        dir,
        WalkDirection::Forward | WalkDirection::ForwardLeft | WalkDirection::ForwardRight
    )
}

/// Replicates azalea `execute_parkour_move`'s jump trigger for the live parkour edge
/// `start -> target` (both feet-level block coords), given the bot's current position. Returns
/// `(jump_now, want_sprint)`. We replicate (rather than let azalea's executor jump) because the
/// controller is the sole authority on the wire — the executor runs after `send_position`, so its
/// own jump/look never reach the server (see module docs). Kept pure → unit-tested below.
///
/// `jump_distance = max(|dx|, |dz|)` is 2 / 3 / 4 for a 1 / 2 / 3-block gap; basic moves are always
/// 1, so callers gate on `>= 2` to know an edge is a parkour edge at all. The bot must be ON the
/// take-off-edge column (`start + dir`) and far enough past the start centre to leave the ground.
pub fn parkour_step(
    start: (i32, i32, i32),
    target: (i32, i32, i32),
    pos: (f64, f64, f64),
) -> (bool, bool) {
    let (sx, sy, sz) = start;
    let (tx, ty, tz) = target;
    let jump_distance = (tx - sx).abs().max((tz - sz).abs());
    let ascend = ty - sy;
    let want_sprint = jump_distance >= 4 || (ascend > 0 && jump_distance >= 3);

    let x_dir = (tx - sx).clamp(-1, 1);
    let z_dir = (tz - sz).clamp(-1, 1);
    let jump_block = (sx + x_dir, sy, sz + z_dir);

    let blk = (
        pos.0.floor() as i32,
        pos.1.floor() as i32,
        pos.2.floor() as i32,
    );
    let is_at_start = blk == (sx, sy, sz);
    let is_at_jump = blk == jump_block;

    let required = if jump_distance <= 2 { 0.0 } else { 0.6 };
    let dist_from_start = (pos.0 - (sx as f64 + 0.5))
        .abs()
        .max((pos.2 - (sz as f64 + 0.5)).abs());

    let jump = !is_at_start && is_at_jump && dist_from_start > required;
    (jump, want_sprint)
}

// ---------------------------------------------------------------------------------------------
// Intents (set by the scenario async task via Arc<BotInner>; read by the ECS systems).
// ---------------------------------------------------------------------------------------------

/// What the camera should do. The scenario sets this; `apply_intents` advances it one quantum/tick.
#[derive(Clone, Copy, Debug, Default)]
pub enum CameraIntent {
    /// Hold the current heading (no rotation). Default when no scenario look is active; the motion
    /// controller may then steer the camera toward travel to keep sprint engaged.
    #[default]
    Hold,
    /// Rotate at fixed rate toward an absolute yaw/pitch (degrees).
    LookAt { yaw: f64, pitch: f64 },
    /// Constant-rate triangle sweep: oscillate yaw within ±`amp_deg` around `base_yaw` at the fixed
    /// rate, for `ticks` ticks. `dir` is the current swing sign.
    Sweep {
        base_yaw: f64,
        amp_deg: f64,
        ticks: u64,
        dir: f64,
    },
}

/// What the motion controller should do.
#[derive(Clone, Copy, Debug, Default)]
pub enum LocoIntent {
    /// Stand still.
    #[default]
    Idle,
    /// Walk straight toward (x, z) (no pathfinder); arrive within `radius`.
    Straight { x: f64, z: f64, radius: f64 },
    /// Follow OUR nav corridor (nav.rs: planner-owned loop over azalea's public
    /// `calculate_path`); arrive at the final goal (x, z) within `radius`. The scenario must
    /// have set the NavState goal (`Ctx::move_to` does). The goal coords are carried so the
    /// controller can swim STRAIGHT toward them when in water: the land planner never routes
    /// through a fluid, so the corridor is useless from inside water and the bot would be
    /// stranded (only its camera moving) without this fallback.
    Planned { x: f64, z: f64, radius: f64 },
    /// Scripted hold (the calibration episode): drive a specific azalea [`WalkDirection`] every tick
    /// directly — NOT derived from a target/pathfinder — with `sprint` requested. Lets the episode
    /// demonstrate one movement key at a time. Jumps still come from `jump_request`.
    Hold { dir: WalkDirection, sprint: bool },
}

/// The discrete action actually applied this tick — recorded verbatim (not read back from
/// components, which azalea may mutate, e.g. one-shot `Jumping` cleared by `stop_jumping`).
#[derive(Clone, Copy, Debug, Default)]
pub struct LastAction {
    /// 8-way move label.
    pub move_dir: WalkDirection,
    pub jump: bool,
    pub sprint: bool,
}

// ---------------------------------------------------------------------------------------------
// Plugin: three GameTick systems, ordered around PhysicsSystems.
// ---------------------------------------------------------------------------------------------

pub struct ControlPlugin;

impl Plugin for ControlPlugin {
    fn build(&self, app: &mut App) {
        // Explicit order: tick_counter -> apply_intents -> PhysicsSystems -> capture. The chain ties
        // tick_counter before apply_intents (a bare tuple has no guaranteed order in Bevy), and
        // both before PhysicsSystems; capture runs after PhysicsSystems — so client_ticks is incremented
        // exactly once before the frame (and SetTime anchoring) reads it.
        app.add_systems(
            GameTick,
            (
                (tick_counter, apply_intents).chain().before(PhysicsSystems),
                capture.after(PhysicsSystems),
            ),
        );
    }
}

/// Single authoritative tick counter: exactly once per GameTick, before anything reads it
/// (`SetTime` world-age anchoring, `go_tick_base` snapshot, and the frame `client_tick`).
fn tick_counter(query: Query<&BotState>) {
    for state in &query {
        state.inner.client_ticks.fetch_add(1, Ordering::SeqCst);
    }
}

#[allow(clippy::type_complexity)]
fn apply_intents(
    shared: Res<Shared>,
    mut query: Query<(
        &BotState,
        &mut LookDirection,
        &mut ClientMovementState,
        &mut Jumping,
        &Physics,
        &Position,
    )>,
) {
    let cfg = &shared.inner.cfg;
    let q_yaw = cfg.yaw_rate_deg_s / TICKS_PER_SEC;
    let q_pitch = cfg.pitch_rate_deg_s / TICKS_PER_SEC;

    for (state, mut look, mut phys_state, mut jumping, physics, position) in &mut query {
        let inner = &state.inner;
        if !inner.recording.load(Ordering::SeqCst) {
            // Pre-go: stand still, don't fight azalea for control.
            continue;
        }
        // Post-finish. The script is done, so there is nothing left to fight over — but the camera
        // keeps rolling until the recording stops, and standing still through that window is where
        // most of the corpus's dead air comes from. Wander a short leash around the finish spot
        // instead, unless ambient mode is switched off (then keep the old stand-still behaviour).
        let finished = inner.finished.load(Ordering::SeqCst);
        if finished && !cfg.ambient_when_finished {
            continue;
        }
        let pos = (position.x, position.y, position.z);
        let mut cam_yaw = inner.cam_yaw.lock();
        let mut cam_pitch = inner.cam_pitch.lock();

        // ---- motion: pick the waypoint, choose an 8-way (body-relative to OUR cam yaw) ----
        // Planned nav follows OUR corridor (nav.rs): nav_tick advances by proximity, replans on
        // stalls, and gives up on unreachable goals. The goal-radius arrival is decided by the
        // scenario's move_to poll, NOT here. Straight nav heads at the final target and stops
        // within `radius`.
        // Dead air has two halves. `finished` is the easy one: the script ended and the recorder
        // is still running. The larger one is a script that never ends — it reaches a terminal
        // branch and loops in it until the episode duration is met, which is where 45-154 s freezes
        // come from. Detect that as "the scenario has issued no intent at all for
        // `ambient_idle_ticks`", and take over the same way.
        let scenario_loco = *inner.loco_intent.lock();
        let scenario_quiet = matches!(scenario_loco, LocoIntent::Idle)
            && matches!(*inner.camera_intent.lock(), CameraIntent::Hold);
        let stalled = if cfg.ambient_idle_ticks > 0 && scenario_quiet && !finished {
            inner.stall_ticks.fetch_add(1, Ordering::SeqCst) + 1 > cfg.ambient_idle_ticks
        } else {
            inner.stall_ticks.store(0, Ordering::SeqCst);
            false
        };
        let ambient = finished || stalled;
        if !ambient {
            // Re-anchor the next stall where it actually happens, not where the last one did.
            *inner.ambient_anchor.lock() = None;
        }
        let loco = if ambient {
            // Leashed wander around where the script left this bot. `Straight` on purpose: it needs
            // no corridor plan (a stalled or finished script's planner goal is stale) and it stops
            // itself within `radius`, so the wander cannot turn into a walk across the map.
            let anchor = *inner.ambient_anchor.lock().get_or_insert(pos);
            let epoch = inner.client_ticks.load(Ordering::SeqCst).max(0) as u64
                / cfg.ambient_switch_ticks as u64;
            let (x, z) = ambient_target(
                shared.inner.engine.seed,
                inner.index,
                anchor,
                epoch,
                cfg.ambient_leash,
            );
            LocoIntent::Straight { x, z, radius: 1.0 }
        } else {
            scenario_loco
        };
        // The land planner never routes THROUGH a fluid, so from inside water the corridor is
        // useless and the bot would be stranded (no waypoint -> no walk -> not even a swim-up,
        // which is gated on `moving`). While in water, ignore the plan and head STRAIGHT for the
        // final goal to cross/exit; corridor-following resumes back on land.
        let in_water = physics.is_in_water();
        let mut nav_out = crate::nav::NavTickOut::default();
        let (waypoint, eps) = match loco {
            LocoIntent::Idle => (None, 1.0),
            // Arrival floors are generous on purpose: data collection needs "near the target",
            // never an exact xyz — precision maneuvering is exactly what generates short
            // corrective key presses.
            LocoIntent::Straight { x, z, radius } => (Some((x, z)), radius.max(1.0)),
            LocoIntent::Planned { x, z, radius } if in_water => (Some((x, z)), radius.max(1.0)),
            LocoIntent::Planned { .. } => {
                let tick = inner.client_ticks.load(Ordering::SeqCst);
                nav_out = crate::nav::nav_tick(&mut inner.nav.lock(), pos, tick);
                // The steering target is a lookahead corridor node several blocks out; the eps
                // only needs to cover "already essentially on it" (cursor advancement is the
                // real progression mechanism, at nav::ADVANCE_RADIUS).
                (nav_out.waypoint, 1.0)
            }
            LocoIntent::Hold { .. } => (None, 1.0), // `chosen` is set directly below, not via a waypoint
        };

        let mut chosen = WalkDirection::None;
        let mut want_sprint = false;
        let wp_dist = waypoint.map(|(tx, tz)| ((tx - pos.0).powi(2) + (tz - pos.2).powi(2)).sqrt());
        // Final-goal distance (NOT the node waypoint — planned nodes are always ~1 block away,
        // which would keep the sprint approach-gate permanently engaged).
        let goal_dist = match loco {
            LocoIntent::Straight { x, z, .. } | LocoIntent::Planned { x, z, .. } => {
                Some(((x - pos.0).powi(2) + (z - pos.2).powi(2)).sqrt())
            }
            _ => None,
        };
        if let LocoIntent::Hold { dir, sprint } = loco {
            // Scripted hold (calibration): drive this key directly, not from a target/pathfinder.
            chosen = dir;
            want_sprint = cfg.allow_sprinting && sprint;
        } else if let Some((tx, tz)) = waypoint {
            let held = inner.move_latch.lock().dir;
            if let Some(dir) = body_relative_dir(tx - pos.0, tz - pos.2, *cam_yaw, eps, held) {
                chosen = dir;
            }
        }
        // ---- latch on the APPLIED 8-way (see `latch_step`): transient-stop debounce + overshoot
        // settle (don't chase a just-passed waypoint backward) + identity-dwell (a direction
        // CHANGE — direct or via a brief stop — needs the previous direction to have stood
        // min_dwell_ticks). Latched here in the apply layer, so the recorder below sees the smoothed
        // direction. Same-direction resumes pass through immediately; a stop does too once its
        // identity has served the dwell, or whenever the waypoint is within REVERSAL_ARRIVE_DIST.
        chosen = latch_step(
            &mut inner.move_latch.lock(),
            chosen,
            waypoint,
            wp_dist,
            goal_dist,
            cfg.min_dwell_ticks,
        );
        if !matches!(loco, LocoIntent::Hold { .. }) {
            // Sprint is an INDEPENDENT per-bout draw (see `SprintBout`), then filtered by the two
            // physical facts that have to hold anyway: Minecraft only sprints forward-ish, and the
            // final approach must decelerate or the bot slides past its waypoint
            // (SPRINT_APPROACH_DIST). Before 2026-08 there was no draw and those two filters WERE
            // the decision, so `sprint` was inferable from position and taught the model nothing.
            let intends_sprint = inner.sprint_bout.lock().tick(
                !matches!(chosen, WalkDirection::None),
                shared.inner.engine.seed,
                inner.index,
                cfg.sprint_bout_prob,
            );
            want_sprint = cfg.allow_sprinting
                && intends_sprint
                && is_forwardish(chosen)
                && goal_dist.is_none_or(|d| d > SPRINT_APPROACH_DIST);
        }
        phys_state.move_direction = chosen;
        let moving = !matches!(chosen, WalkDirection::None);

        // controller owns ALL jumps (azalea's executor runs after `send_position`, so its jumps
        // never reach the wire — see module docs). Cases, in priority order:
        //  (0) parkour gap-jump (opt-in `allow_parkour`): the live edge spans a gap (horizontal
        //      span >= 2 — basic edges are always 1); replicate azalea's take-off timing,
        //  (1) pre-contact uphill: planned next node is a full block higher,
        //  (2) contact step: blocked horizontally while grounded,
        //  (3) water: jump = swim up.
        let want_jump = if let Some((s, t)) = nav_out.parkour_edge {
            let (jump, sprint) = parkour_step(s, t, pos);
            // Replicate azalea's parkour sprint EXACTLY: a 3-block (or ascending 2-block) gap MUST
            // be sprint-jumped or it can't be cleared, and azalea always sprints these. So engage
            // sprint purely on the parkour predicate — NOT gated by `allow_sprinting` (opting into
            // `allow_parkour` opts into the sprint its jumps require) nor by `is_forwardish`
            // (azalea's ai_step only sprints on ~pure-forward motion anyway, so an unconditional
            // write is a safe no-op while the camera is mid-turn). Walk-jumps keep sprint=false.
            want_sprint = sprint;
            jump
        } else {
            // Swim up whenever in water — NOT gated on `moving` — so the bot floats at the surface and
            // never sinks/strands during a look-only phase; otherwise jump only to climb or step up.
            in_water
                || (moving
                    && (nav_out.uphill || (physics.on_ground() && physics.horizontal_collision)))
        };
        // Scenario-commanded jump (e.g. build_tower_underneath's pillar-jump): the controller is
        // the sole wire authority, so world primitives that need a jump raise `jump_request`
        // instead of calling `bot.jump()` (whose JumpEvent the executor-ordering would clobber).
        let want_jump = want_jump || inner.jump_request.load(Ordering::SeqCst);
        phys_state.trying_to_sprint = want_sprint;
        **jumping = want_jump;

        // ---- camera: advance OUR owned heading by one quantum toward the intent, then ALWAYS
        //      write LookDirection from it (overriding any live pathfinder look-at snap) ----
        let (target_yaw, target_pitch, sweep_next) = match *inner.camera_intent.lock() {
            CameraIntent::LookAt { yaw, pitch } => (yaw, pitch, None),
            CameraIntent::Sweep {
                base_yaw,
                amp_deg,
                ticks,
                dir,
            } => {
                // triangle: aim at the far edge in the current swing direction; flip at the edge.
                let edge = base_yaw + dir * amp_deg;
                let flip = shortest_arc_deg(edge - *cam_yaw).abs() <= q_yaw;
                let next = CameraIntent::Sweep {
                    base_yaw,
                    amp_deg,
                    ticks: ticks.saturating_sub(1),
                    dir: if flip { -dir } else { dir },
                };
                (edge, *cam_pitch, Some(next))
            }
            CameraIntent::Hold => {
                // No scenario look: face travel so sprint can engage (documented coupling).
                if let (Some((tx, tz)), true) = (waypoint, moving) {
                    let (ty, _) = look_angles(pos, (tx, pos.1, tz));
                    (ty, *cam_pitch, None)
                } else {
                    (*cam_yaw, *cam_pitch, None)
                }
            }
        };

        let (dyaw, ylab) = quantize_step(shortest_arc_deg(target_yaw - *cam_yaw), q_yaw);
        let (dpitch, plab) = quantize_step(target_pitch - *cam_pitch, q_pitch);
        *cam_yaw += dyaw;
        *cam_pitch = (*cam_pitch + dpitch).clamp(-90.0, 90.0);
        look.update(LookDirection::new(*cam_yaw as f32, *cam_pitch as f32));

        // advance / settle sweep + camera-busy flag for the scenario to poll
        if let Some(next) = sweep_next {
            *inner.camera_intent.lock() = match next {
                CameraIntent::Sweep { ticks: 0, .. } => CameraIntent::Hold,
                other => other,
            };
        }
        let cam_busy = matches!(*inner.camera_intent.lock(), CameraIntent::Sweep { .. })
            || ylab != 0
            || plab != 0;
        inner.camera_busy.store(cam_busy, Ordering::SeqCst);

        // record the discrete action we just applied (authoritative for the frame labels)
        *inner.last_action.lock() = LastAction {
            move_dir: chosen,
            jump: want_jump,
            sprint: want_sprint,
        };
    }
}

/// Dense per-tick recorder. Runs AFTER PhysicsSystems so pose/velocity are this tick's post-physics
/// state (what the replay/video frame shows); labels come from `LastAction` (our decision).
#[allow(clippy::type_complexity)]
fn capture(
    query: Query<(
        &BotState,
        &Position,
        &ClientMovementState,
        &Physics,
        Option<&Inventory>,
        Option<&Mining>,
    )>,
) {
    for (state, position, phys_state, physics, inventory, mining) in &query {
        let inner = &state.inner;
        if !inner.recording.load(Ordering::SeqCst) || inner.finished.load(Ordering::SeqCst) {
            continue;
        }
        let pos = (position.x, position.y, position.z);
        *inner.live_pos.lock() = Some(pos);

        // Record OUR authoritative camera (what send_position transmitted this tick = what the
        // video shows), never the raw LookDirection (a stray executor snap could land between
        // PhysicsSystems and this system).
        let yaw = shortest_arc((*inner.cam_yaw.lock()).to_radians());
        let pitch = (*inner.cam_pitch.lock()).to_radians();
        let sneak = phys_state.trying_to_crouch;
        let act = *inner.last_action.lock();
        // On azalea 0.16 the wire direction IS the visual direction (see STRAFE_SIGN); record it
        // directly. Any future strafe-handedness change shows up in the calibration scenario.
        let vis = act.move_dir;
        let forward = matches!(
            vis,
            WalkDirection::Forward | WalkDirection::ForwardLeft | WalkDirection::ForwardRight
        );
        let back = matches!(
            vis,
            WalkDirection::Backward | WalkDirection::BackwardLeft | WalkDirection::BackwardRight
        );
        let left = matches!(
            vis,
            WalkDirection::Left | WalkDirection::ForwardLeft | WalkDirection::BackwardLeft
        );
        let right = matches!(
            vis,
            WalkDirection::Right | WalkDirection::ForwardRight | WalkDirection::BackwardRight
        );
        let (held_item, hotbar_slot) = inventory
            .map(|inv| (item_name(inv.held_item()), Some(inv.selected_hotbar_slot)))
            .unwrap_or((None, None));
        let inventory_snapshot = inventory.map(inventory_snapshot);

        let mut last_yaw = inner.last_yaw.lock();
        let mut last_pitch = inner.last_pitch.lock();
        let mut pending = inner.pending.lock();
        let server_tick = inner.offset_tick();
        let frame = ActionFrame {
            client_tick: ClientTick(inner.client_ticks.load(Ordering::SeqCst)),
            server_tick: ServerTick(server_tick),
            world_age_tick: inner.world_age_tick().map(WorldAgeTick),
            player_time_ms: PlayerTimeMs(inner.elapsed_ms().max(0)),
            // Raw f64 all the way to the dataset row: solaris's recorder ships the bot's
            // doubles unquantized, and the v3 bar is byte-level value parity (code review
            // caught the old r3/r4 grid in the shipped rows).
            x: pos.0,
            y: pos.1,
            z: pos.2,
            yaw,
            pitch,
            d_yaw: shortest_arc(yaw - *last_yaw),
            d_pitch: pitch - *last_pitch,
            keys: MovementKeys {
                forward,
                back,
                left,
                right,
                jump: act.jump,
                sprint: act.sprint,
                sneak,
            },
            attack: pending.attack,
            use_item: pending.use_item,
            place_block: pending.place_block,
            // Sustained, like solaris (its consumer maps mine to a held LMB): true on every tick
            // the break is in progress — azalea's `Mining` component exists exactly while a block
            // is being broken. The `pending.mine` pulse covers the initiating tick, where the
            // component may not be inserted yet this GameTick.
            mine: mining.is_some() || pending.mine,
            hotbar_slot,
            held_item,
            is_in_water: physics.is_in_water(),
            inventory: inventory_snapshot,
        };
        inner.frames.lock().push(frame);
        inner
            .sep_samples
            .lock()
            .push((server_tick, r3(pos.0), r3(pos.2)));
        *last_yaw = yaw;
        *last_pitch = pitch;
        *pending = crate::Pending::default();
    }
}

/// Full inventory snapshot in the dataset's solaris shape. Azalea's Player menu uses the vanilla
/// window indices (0 craft result, 1–4 craft, 5–8 armor, 9–35 main, 36–44 hotbar, 45 off-hand) —
/// the same ones solaris's recorder groups by, so the mapping is the identity.
fn inventory_snapshot(inv: &Inventory) -> InventorySnapshot {
    let menu = inv.menu();
    let slots = menu.slots();
    let snap = |idx: usize| -> Option<ItemSnapshot> { item_snapshot(slots.get(idx)?, idx as i32) };
    let hotbar_start = *menu.hotbar_slots_range().start();
    InventorySnapshot {
        quick_bar_slot: inv.selected_hotbar_slot,
        main_inventory: (9..=35).filter_map(snap).collect(),
        hotbar: (36..=44).filter_map(snap).collect(),
        held_item: snap(hotbar_start + inv.selected_hotbar_slot as usize),
        off_hand_item: snap(45),
    }
}

/// One stack in prismarine-item's serialized shape (see [`ItemSnapshot`]): protocol id, registry
/// name, title-cased display name, per-item max stack size from azalea's default components.
/// `metadata`/`nbt`/`stackId` are the constants prismarine emits for plain Java items — the kits
/// hand out nothing enchanted or damaged.
fn item_snapshot(stack: &ItemStack, slot: i32) -> Option<ItemSnapshot> {
    let name = item_name(stack)?; // None for an empty stack
    let kind = stack.kind();
    Some(ItemSnapshot {
        type_id: kind as u32,
        count: stack.count(),
        metadata: 0,
        nbt: None,
        stack_id: None,
        display_name: title_case(&name),
        stack_size: MaxStackSize::default_for_item(kind)
            .map(|size| size.count)
            .unwrap_or(64),
        name,
        slot,
    })
}

/// `"stone_bricks"` → `"Stone Bricks"` — prismarine's `displayName` for plain vanilla items.
fn title_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for (i, word) in name.split('_').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Tests (pure logic only; run via `just test` — the crate needs the pinned nightly toolchain and
// the full azalea tree, so these do not build on a stable-only machine).
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantize_step_rounds_at_half_quantum() {
        let q = 3.6;
        // far: full quantum
        assert_eq!(quantize_step(10.0, q), (3.6, 1));
        assert_eq!(quantize_step(-10.0, q), (-3.6, -1));
        // residual just over half -> a real (sub-quantum) action
        let (a, l) = quantize_step(2.0, q);
        assert!((a - 2.0).abs() < 1e-9 && l == 1 && a > q / 2.0);
        // residual at/under half -> settle (no-op)
        assert_eq!(quantize_step(1.8, q), (0.0, 0));
        assert_eq!(quantize_step(-1.7, q), (0.0, 0));
        // invariant: label != 0  iff  |applied| > q/2
        for &rem in &[-12.0, -3.6, -1.9, -1.8, 0.0, 1.0, 1.8, 1.9, 3.6, 12.0] {
            let (a, l) = quantize_step(rem, q);
            assert_eq!(l != 0, a.abs() > q / 2.0, "rem={rem}");
            assert!(a.abs() <= q + 1e-9);
        }
    }

    /// No held direction (fresh start): nearest-sector behavior, no hysteresis.
    fn dir_of(dx: f64, dz: f64, yaw: f64, eps: f64) -> Option<WalkDirection> {
        body_relative_dir(dx, dz, yaw, eps, WalkDirection::None)
    }

    #[test]
    fn body_relative_forward_and_arrive() {
        // Facing +Z (yaw 0 = south): a target straight ahead (+Z) is Forward.
        assert_eq!(dir_of(0.0, 5.0, 0.0, 1.5), Some(WalkDirection::Forward));
        // Target directly behind (−Z) is Backward.
        assert_eq!(dir_of(0.0, -5.0, 0.0, 1.5), Some(WalkDirection::Backward));
        // Within eps → arrived (stand still).
        assert_eq!(dir_of(0.5, 0.5, 0.0, 1.5), None);
    }

    #[test]
    fn body_relative_rotates_with_camera() {
        // Same world target (+Z ahead), but camera yawed 90°: the body-relative key must change
        // away from pure Forward (decoupling: keys are relative to where we look).
        let straight = dir_of(0.0, 5.0, 0.0, 1.0);
        let yawed = dir_of(0.0, 5.0, 90.0, 1.0);
        assert_eq!(straight, Some(WalkDirection::Forward));
        assert_ne!(yawed, Some(WalkDirection::Forward));
        // L+R / F+B can never co-occur: a single WalkDirection encodes at most one of each axis.
        for yaw in [0.0, 45.0, 90.0, 137.0, 200.0, 359.0] {
            let d = dir_of(3.0, -2.0, yaw, 1.0).unwrap();
            assert_ne!(d, WalkDirection::None);
        }
    }

    #[test]
    fn sector_hysteresis_holds_near_boundary() {
        // Bearing just past the F/FR boundary (22.5°): a held Forward is KEPT (within the
        // Schmitt band), while a fresh start snaps to ForwardRight.
        // yaw 0 faces +Z; bearing ~27° to the (visual-sign) right.
        let ang: f64 = 27.0_f64.to_radians();
        let (dx, dz) = {
            // world delta whose body bearing is +27°: rotate forward (+Z) by the bearing with
            // the STRAFE_SIGN mirror baked in (right component sign).
            let fwd = ang.cos();
            let right = ang.sin() * STRAFE_SIGN; // undo the mirror applied in body_bearing_deg
            (-right, fwd)
        };
        assert_eq!(
            body_relative_dir(dx, dz, 0.0, 0.1, WalkDirection::None),
            Some(WalkDirection::ForwardRight)
        );
        assert_eq!(
            body_relative_dir(dx, dz, 0.0, 0.1, WalkDirection::Forward),
            Some(WalkDirection::Forward)
        );
        // Well past the band (22.5 + 10): held Forward finally yields.
        let ang2: f64 = 35.0_f64.to_radians();
        let (dx2, dz2) = {
            let fwd = ang2.cos();
            let right = ang2.sin() * STRAFE_SIGN;
            (-right, fwd)
        };
        assert_eq!(
            body_relative_dir(dx2, dz2, 0.0, 0.1, WalkDirection::Forward),
            Some(WalkDirection::ForwardRight)
        );
    }

    #[test]
    fn look_angles_pitch_clamped() {
        let (_, pitch) = look_angles((0.0, 64.0, 0.0), (0.0, 9999.0, 0.0));
        assert!(pitch >= -90.0);
        let (_, pitch2) = look_angles((0.0, 64.0, 0.0), (0.0, -9999.0, 0.0));
        assert!(pitch2 <= 90.0);
    }

    /// Drive `latch_step` one tick with a stationary far waypoint (no settle interference).
    fn step(latch: &mut MoveLatch, req: WalkDirection, dwell: u32) -> WalkDirection {
        latch_step(
            latch,
            req,
            Some((100.0, 0.0)),
            Some(100.0),
            Some(100.0),
            dwell,
        )
    }

    /// Same, but with the waypoint inside `REVERSAL_ARRIVE_DIST`: an arrival, where a stop is real.
    fn step_near(latch: &mut MoveLatch, req: WalkDirection, dwell: u32) -> WalkDirection {
        latch_step(latch, req, Some((1.0, 0.0)), Some(1.0), Some(1.0), dwell)
    }

    /// The case the first debounce got wrong: a lookahead corridor NODE one block ahead while the
    /// episode's real goal is still far. `nav::ADVANCE_RADIUS` is 1.5, so this is what Planned nav
    /// looks like on nearly every tick of a long walk, and a transient stop here must be absorbed.
    fn step_node(latch: &mut MoveLatch, req: WalkDirection, dwell: u32) -> WalkDirection {
        latch_step(latch, req, Some((1.0, 0.0)), Some(1.0), Some(80.0), dwell)
    }

    #[test]
    fn latch_suppresses_changes_before_dwell() {
        let mut l = MoveLatch::default();
        assert_eq!(
            step(&mut l, WalkDirection::Forward, 5),
            WalkDirection::Forward
        );
        // Mid-dwell switch to a different non-stop dir: keep holding the current one (this is
        // the pathfinder wiggle we're smoothing out).
        assert_eq!(step(&mut l, WalkDirection::Left, 5), WalkDirection::Forward);
        assert_eq!(
            step(&mut l, WalkDirection::ForwardLeft, 5),
            WalkDirection::Forward
        );
        // Re-requesting the held dir is never a change.
        assert_eq!(
            step(&mut l, WalkDirection::Forward, 5),
            WalkDirection::Forward
        );
    }

    #[test]
    fn latch_allows_changes_after_dwell() {
        let mut l = MoveLatch::default();
        for _ in 0..5 {
            assert_eq!(
                step(&mut l, WalkDirection::Forward, 5),
                WalkDirection::Forward
            );
        }
        assert_eq!(step(&mut l, WalkDirection::Left, 5), WalkDirection::Left);
        for _ in 0..9 {
            step(&mut l, WalkDirection::Left, 5);
        }
        assert_eq!(
            step(&mut l, WalkDirection::Backward, 5),
            WalkDirection::Backward
        );
    }

    #[test]
    fn latch_honours_an_arrival_stop_and_a_same_dir_resume() {
        let mut l = MoveLatch::default();
        step_near(&mut l, WalkDirection::Forward, 5);
        // Near the waypoint a stop is a real decision: immediate, so arrival cannot overshoot.
        assert_eq!(
            step_near(&mut l, WalkDirection::None, 5),
            WalkDirection::None
        );
        // Resuming the SAME direction after a stop is always immediate.
        assert_eq!(
            step_near(&mut l, WalkDirection::Forward, 5),
            WalkDirection::Forward
        );
    }

    #[test]
    fn latch_defers_a_transient_stop_far_from_the_waypoint() {
        // The 2026-08 defect: a one-tick `None` mid-bout ended the key press, so 27-49% of
        // movement-key presses lasted under the 0.5 s dwell floor and the shortest bouts moved the
        // player a median 0.43 blocks. Far from the waypoint such a stop is now held off.
        let mut l = MoveLatch::default();
        assert_eq!(
            step(&mut l, WalkDirection::Forward, 5),
            WalkDirection::Forward
        );
        // Ticks 2..5 of the Forward identity: the stop is absorbed and the key stays down.
        for tick in 2..=5 {
            assert_eq!(
                step(&mut l, WalkDirection::None, 5),
                WalkDirection::Forward,
                "tick {tick} should still be holding Forward"
            );
        }
        // Once the identity has stood its dwell, a stop is honoured like any other change.
        assert_eq!(step(&mut l, WalkDirection::None, 5), WalkDirection::None);
    }

    #[test]
    fn latch_defers_a_transient_stop_at_a_corridor_node_while_the_goal_is_far() {
        // The regression an earlier validation capture exposed. Planned nav steers at a
        // lookahead node ~1 block out, so gating the debounce on the NODE distance disabled it on
        // nearly every tick of a long walk and short presses got WORSE (forward 27.6% -> 44.7%,
        // left 32.7% -> 93.1%). The gate reads the FINAL-goal distance, so this case must absorb.
        let mut l = MoveLatch::default();
        assert_eq!(
            step_node(&mut l, WalkDirection::Forward, 5),
            WalkDirection::Forward
        );
        for _ in 0..4 {
            assert_eq!(
                step_node(&mut l, WalkDirection::None, 5),
                WalkDirection::Forward,
                "a node-adjacent stop mid-dwell must not end the press while the goal is far"
            );
        }
        assert_eq!(
            step_node(&mut l, WalkDirection::None, 5),
            WalkDirection::None
        );
    }

    #[test]
    fn sprint_bout_is_independent_reproducible_and_bounded() {
        // Deterministic: the same (seed, bot, bout) always decides the same way, so a re-render of
        // an episode reproduces its action stream.
        assert_eq!(
            sprint_for_bout(1234, 3, 7, 0.5),
            sprint_for_bout(1234, 3, 7, 0.5)
        );
        // The two degenerate probabilities are exact, so a run can be made walk-only or
        // always-sprint without touching the controller.
        assert!(!sprint_for_bout(1234, 3, 7, 0.0));
        assert!(sprint_for_bout(1234, 3, 7, 1.0));
        // Independent of the navigation state means: over many bouts, both cases actually occur.
        // This is the property the shipped corpus lacked -- only 20 sustained walk-without-sprint
        // runs in 487,160 frames.
        let sprinted = (0..400).filter(|b| sprint_for_bout(99, 1, *b, 0.5)).count();
        assert!(
            (120..=280).contains(&sprinted),
            "expected a mix of sprint and walk bouts, got {sprinted}/400"
        );
        // Different bots do not march in lockstep.
        let a: Vec<bool> = (0..40).map(|b| sprint_for_bout(7, 0, b, 0.5)).collect();
        let b: Vec<bool> = (0..40).map(|b| sprint_for_bout(7, 1, b, 0.5)).collect();
        assert_ne!(a, b);
    }

    #[test]
    fn sprint_bout_holds_its_decision_for_the_whole_bout() {
        let mut bout = SprintBout::default();
        // Find a seed whose first bout sprints, so the assertion is about holding, not about luck.
        let seed = (0..64u32)
            .find(|s| sprint_for_bout(*s, 0, 1, 0.5))
            .expect("some seed sprints on its first bout");
        let first = bout.tick(true, seed, 0, 0.5);
        assert!(first);
        for _ in 0..50 {
            assert_eq!(bout.tick(true, seed, 0, 0.5), first);
        }
        assert_eq!(bout.bouts, 1, "a continuous bout must not be recounted");
        // Stopping ends the bout and clears the intent; moving again is a NEW draw.
        assert!(!bout.tick(false, seed, 0, 0.5));
        bout.tick(true, seed, 0, 0.5);
        assert_eq!(bout.bouts, 2);
    }

    #[test]
    fn ambient_target_stays_inside_the_leash_and_moves_between_epochs() {
        let anchor = (10.0, 64.0, -20.0);
        let leash = 4.0;
        for epoch in 0..64 {
            let (x, z) = ambient_target(42, 2, anchor, epoch, leash);
            let d = ((x - anchor.0).powi(2) + (z - anchor.2).powi(2)).sqrt();
            assert!(d <= leash + 1e-9, "epoch {epoch} escaped the leash: {d}");
        }
        // Successive epochs must actually differ, or the "wander" is another statue.
        let first = ambient_target(42, 2, anchor, 0, leash);
        let second = ambient_target(42, 2, anchor, 1, leash);
        assert_ne!(first, second);
        // A zero leash is a legal way to ask for the old stand-still behaviour.
        assert_eq!(ambient_target(42, 2, anchor, 5, 0.0), (anchor.0, anchor.2));
    }

    #[test]
    fn latch_zero_dwell_still_stops_instantly() {
        // min_dwell == 0 disables the latch entirely, debounce included.
        let mut l = MoveLatch::default();
        step(&mut l, WalkDirection::Forward, 0);
        assert_eq!(step(&mut l, WalkDirection::None, 0), WalkDirection::None);
    }

    #[test]
    fn latch_dwell_survives_stop_gaps() {
        // The v1 loophole: W -> none(1 tick) -> S switched instantly because starts from stop were
        // unconditional. Two mechanisms now close it, and which one fires depends on where the
        // waypoint is. FAR from it the 2026-08 transient-stop debounce absorbs the gap outright
        // (asserted in `latch_defers_a_transient_stop_far_from_the_waypoint`). NEXT to it the stop is
        // real, so the dwell clock — which follows the last non-stop identity through the gap — and
        // the overshoot settle both hold the reversal off. This test asserts the invariant the
        // loophole broke, not a tick count: no instant flip, and the reversal still eventually lands.
        let mut l = MoveLatch::default();
        for _ in 0..2 {
            step_near(&mut l, WalkDirection::Forward, 5);
        }
        assert_eq!(
            step_near(&mut l, WalkDirection::None, 5),
            WalkDirection::None
        );
        assert_eq!(
            step_near(&mut l, WalkDirection::Backward, 5),
            WalkDirection::None,
            "a reversal one tick after a stop must never be instant"
        );
        let landed = (0..SETTLE_TICKS + 10).any(|_| {
            matches!(
                step_near(&mut l, WalkDirection::Backward, 5),
                WalkDirection::Backward
            )
        });
        assert!(landed, "the reversal must be allowed once the wait expires");
    }

    #[test]
    fn latch_zero_dwell_is_passthrough() {
        let mut l = MoveLatch::default();
        assert_eq!(
            step(&mut l, WalkDirection::Forward, 0),
            WalkDirection::Forward
        );
        assert_eq!(step(&mut l, WalkDirection::Left, 0), WalkDirection::Left);
        assert_eq!(step(&mut l, WalkDirection::None, 0), WalkDirection::None);
    }

    #[test]
    fn sector_gap_measures_turns() {
        assert_eq!(
            sector_gap(WalkDirection::Forward, WalkDirection::Forward),
            Some(0)
        );
        assert_eq!(
            sector_gap(WalkDirection::Forward, WalkDirection::ForwardLeft),
            Some(1)
        );
        assert_eq!(
            sector_gap(WalkDirection::Forward, WalkDirection::Left),
            Some(2)
        );
        assert_eq!(
            sector_gap(WalkDirection::Forward, WalkDirection::BackwardLeft),
            Some(3)
        );
        assert_eq!(
            sector_gap(WalkDirection::Forward, WalkDirection::Backward),
            Some(4)
        );
        assert_eq!(
            sector_gap(WalkDirection::ForwardLeft, WalkDirection::BackwardRight),
            Some(4)
        );
        assert_eq!(
            sector_gap(WalkDirection::None, WalkDirection::Forward),
            None
        );
    }

    #[test]
    fn overshoot_settles_instead_of_reversing() {
        let mut l = MoveLatch::default();
        let wp = Some((1.0, 0.0));
        // Traveling forward at the waypoint, long past the dwell.
        for _ in 0..10 {
            assert_eq!(
                latch_step(&mut l, WalkDirection::Forward, wp, Some(5.0), Some(99.0), 5),
                WalkDirection::Forward
            );
        }
        // Walked past it: the request flips 180° with the waypoint 0.8 blocks behind. Without
        // the settle this would be applied immediately (dwell long expired) — the v1 limit
        // cycle. Expect None for SETTLE_TICKS instead.
        for _ in 0..SETTLE_TICKS {
            assert_eq!(
                latch_step(
                    &mut l,
                    WalkDirection::Backward,
                    wp,
                    Some(0.8),
                    Some(99.0),
                    5
                ),
                WalkDirection::None
            );
        }
        // Settle armed once per waypoint: after it expires the gentle re-approach is allowed.
        assert_eq!(
            latch_step(
                &mut l,
                WalkDirection::Backward,
                wp,
                Some(0.8),
                Some(99.0),
                5
            ),
            WalkDirection::Backward
        );
    }

    #[test]
    fn settle_rearms_on_new_waypoint_only() {
        let mut l = MoveLatch::default();
        let wp1 = Some((1.0, 0.0));
        for _ in 0..10 {
            latch_step(
                &mut l,
                WalkDirection::Forward,
                wp1,
                Some(5.0),
                Some(99.0),
                5,
            );
        }
        assert_eq!(
            latch_step(
                &mut l,
                WalkDirection::Backward,
                wp1,
                Some(0.8),
                Some(99.0),
                5
            ),
            WalkDirection::None
        );
        // Drain the settle, then re-approach; the SAME waypoint may not arm again.
        for _ in 0..SETTLE_TICKS {
            latch_step(
                &mut l,
                WalkDirection::Backward,
                wp1,
                Some(0.8),
                Some(99.0),
                5,
            );
        }
        assert_eq!(
            latch_step(
                &mut l,
                WalkDirection::Backward,
                wp1,
                Some(0.8),
                Some(99.0),
                5
            ),
            WalkDirection::Backward
        );
        // A NEW waypoint (node advanced) re-arms the protection.
        for _ in 0..10 {
            latch_step(
                &mut l,
                WalkDirection::Backward,
                wp1,
                Some(0.8),
                Some(99.0),
                5,
            );
        }
        let wp2 = Some((3.0, 0.0));
        assert_eq!(
            latch_step(
                &mut l,
                WalkDirection::Forward,
                wp2,
                Some(1.0),
                Some(99.0),
                5
            ),
            WalkDirection::None
        );
    }

    #[test]
    fn parkour_jumps_at_takeoff_edge() {
        // 1-block gap (+X): start (0,64,0) -> target (2,64,0). jump_distance 2 => walk-jump.
        let s = (0, 64, 0);
        let t = (2, 64, 0);
        // standing centred on the start block: do NOT jump yet (walk to the edge first).
        assert_eq!(parkour_step(s, t, (0.5, 64.0, 0.5)), (false, false));
        // stepped onto the take-off column (start+dir): jump now (required dist = 0 for a 1-gap).
        assert_eq!(parkour_step(s, t, (1.5, 64.0, 0.5)), (true, false));

        // 3-block gap (+X): target 4 away => sprint, and must be 0.6 past the start centre.
        let t3 = (4, 64, 0);
        let (_, sprint) = parkour_step(s, t3, (1.5, 64.0, 0.5));
        assert!(sprint, "3-block gap must sprint");
        assert!(!parkour_step(s, t3, (1.05, 64.0, 0.5)).0); // 0.55 past < 0.6 -> wait
        assert!(parkour_step(s, t3, (1.5, 64.0, 0.5)).0); //  1.0 past  > 0.6 -> jump

        // 2-block ascending gap: jump_distance 3 + ascend => sprint.
        assert!(parkour_step((0, 64, 0), (3, 65, 0), (1.5, 64.0, 0.5)).1);
    }
}

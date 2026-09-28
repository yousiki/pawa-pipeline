//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=false scenery=none far_players=false
//!
//! One player demonstrates a short step figure while the other watches it, then the two swap and the
//! watcher replays the SAME figure while the demonstrator watches. Strictly one at a time.
//!
//! The revisit-problem face is the teammate that walks into frame having never appeared in the context
//! frame (22.4% of v5's multi-player windows). A lesson forces the opposite: the demonstrator's whole
//! reason to be there is to be watched, and it keeps the student framed while it performs, so both
//! POVs hold the other body continuously. The pairing is also the cleanest cross-view supervision
//! this corpus can offer without any world edits at all — the same physical action, performed twice,
//! recorded from two viewpoints and two roles, with the figure identity written into the action log.
//!
//! The gaze loop is per step: acquire the partner, hold the view pinned through the step, re-acquire.
//! The trajectory loop is the figure vocabulary itself — every figure returns roughly to where it
//! started (a left is answered by a right, an advance by a retreat), so displacement stays small
//! while path length keeps growing.
//!
//! Nothing is placed or dug: this is the one scenario of the seven whose subject is purely the other
//! player, so a window's ground truth is another body's pose rather than a structure.
//!
//! Not `mirror_dance`: there both partners dance the same beat SIMULTANEOUSLY off a shared hash, and
//! the near-clones risk is managed with per-bot jitter. Here the two performances never overlap in
//! time at all — which is both a cleaner separation of the two action streams and the thing that
//! makes the imitation legible as imitation.

use super::*;

/// The step vocabulary a lesson is drawn from: visual direction plus press duration. Every press is
/// at least 750 ms on purpose — the key-chatter gate fails an episode when over 35% of movement
/// presses run shorter than 10 ticks (0.5 s), and a lesson is nothing but presses.
///
/// Every figure is BALANCED: each direction is answered by its opposite or by a symmetric pair, so a
/// performance returns near its start and net displacement stays inside a couple of blocks while path
/// length grows with every lesson.
const FIGURES: [&[(WalkDirection, u64)]; 4] = [
    &[
        (WalkDirection::Left, 900),
        (WalkDirection::Right, 900),
        (WalkDirection::Backward, 800),
        (WalkDirection::Forward, 800),
    ],
    &[
        (WalkDirection::Backward, 1_000),
        (WalkDirection::Left, 800),
        (WalkDirection::Forward, 1_000),
        (WalkDirection::Right, 800),
    ],
    &[
        (WalkDirection::Right, 1_100),
        (WalkDirection::Left, 1_100),
        (WalkDirection::Right, 750),
        (WalkDirection::Left, 750),
    ],
    &[
        (WalkDirection::Forward, 850),
        (WalkDirection::Left, 950),
        (WalkDirection::Backward, 850),
        (WalkDirection::Right, 950),
    ],
];

const FIGURE_NAMES: [&str; 4] = ["side_to_side", "box_step", "double_shuffle", "diamond"];

/// Half the distance between the two marks. 4.5 puts the pair 9 blocks apart, which leaves room for a
/// figure's advance step without closing inside [`MIN_PEER_GAP`], and keeps the partner large enough
/// in frame that its pose is actually readable.
const MARK_HALF: f64 = 4.5;

/// Length of one HALF lesson — one performance plus the watching that frames it. Four presses plus
/// two confirmed re-acquires run ~6 s, so 7.5 s leaves slack without letting the watcher idle.
const HALF_MS: f64 = 7_500.0;

/// Tolerance for "the partner is in frame" — well inside the 51 deg half-FOV, so a confirmed
/// re-acquire really centres the body rather than catching it at the edge.
const PEER_TOL_DEG: f64 = 8.0;

/// Shortest movement key press this script issues, including the watcher's framing legs.
const MIN_PRESS_MS: u64 = 700;

/// Gap below which an advance step is swapped for a retreat, and above which a retreat becomes an
/// advance: the figure is preserved in shape but never walks the pair into each other's head box.
const GAP_TIGHT: f64 = 6.0;
const GAP_LOOSE: f64 = 12.0;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded varies
/// per bot and per lesson and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// splitmix64-style mix: the shared lesson plan. Both bots evaluate it on the same lesson number and
/// get the same figure with no messaging and no barrier, which is what makes the second performance a
/// genuine repeat of the first rather than a coincidence.
fn mix(x: u64) -> u64 {
    let mut x = x.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x.wrapping_mul(0x94D0_49BB_1331_11EB)
}

/// The pair's shared axis, agreed with NO messaging: the engine hands every bot the same
/// `native_waypoints` list, so its first entry is a direction both sides derive identically.
fn shared_axis(ctx: &Ctx) -> (f64, f64) {
    let waypoint = ctx
        .engine()
        .shared
        .native_waypoints
        .first()
        .copied()
        .unwrap_or((1.0, 0.0));
    let span = waypoint.0.hypot(waypoint.1).max(0.001);
    // Keep the two roles of each pair aligned while rotating each pair's workspace for N=4+.
    let angle = ctx.index().saturating_div(2) as f64 * 0.83;
    let (sin, cos) = angle.sin_cos();
    (
        waypoint.0 / span * cos - waypoint.1 / span * sin,
        waypoint.0 / span * sin + waypoint.1 / span * cos,
    )
}

/// 2D distance to the partner, or `None` when its position has not replicated yet.
fn peer_gap(ctx: &Ctx, partner: Option<usize>) -> Option<f64> {
    let own = ctx.position()?;
    let peer = ctx.peer_position(partner?)?;
    Some((peer.0 - own.0).hypot(peer.2 - own.2))
}

/// Turn onto the partner's head and CONFIRM the view arrived. `look` only waits out a deadline, so a
/// wide turn that timed out would be recorded as a re-acquire that never happened.
async fn face_partner(ctx: &Ctx, partner: Option<usize>, fallback: (f64, f64, f64)) -> bool {
    let target = partner
        .and_then(|j| ctx.peer_position(j))
        .map(head_of)
        .unwrap_or(fallback);
    ctx.look_until_facing(target, PEER_TOL_DEG, 4_000).await
}

/// Run one step of a figure with the camera PINNED where `face_partner` left it.
///
/// `hold_visual` sets `CameraIntent::LookAt` at the current angles and drives one key directly with
/// no pathfinder waypoint, so the view does not move for the duration — the partner stays framed
/// through the step, and the step contributes no turning-while-moving frames to the
/// camera/travel-independence gate (a tracked look while walking measured 96-100% there).
///
/// Returns whether the press ran to term.
async fn step(ctx: &mut Ctx, partner: Option<usize>, dir: WalkDirection, ms: u64) -> bool {
    // Preserve the figure's shape but never its collision: swap an advance for a retreat when the
    // pair is already tight, and the other way round when it has drifted apart.
    let dir = match (dir, peer_gap(ctx, partner)) {
        (WalkDirection::Forward, Some(gap)) if gap < GAP_TIGHT => WalkDirection::Backward,
        (WalkDirection::Backward, Some(gap)) if gap > GAP_LOOSE => WalkDirection::Forward,
        (dir, _) => dir,
    };
    if ctx.hold_visual(dir, false, ms.max(MIN_PRESS_MS), 0).await {
        return true;
    }
    // The stall guard cut the press against terrain. Re-pressing is what turned every step into a
    // sub-0.5 s press in `mirror_dance`'s round-2 key-chatter failure (37% against a 35% ceiling), so
    // abandon the rest of the figure and take ONE pathfinder leg to ground the planner can solve.
    if let Some(own) = ctx.position() {
        let bearing = vary(ctx, 0.0, std::f64::consts::TAU);
        let desired = (own.0 + bearing.cos() * 4.0, own.2 + bearing.sin() * 4.0);
        if let Some(target) = walkable_xz_near(ctx, desired, own.1.floor() as i32) {
            ctx.move_to(target.0, target.1, 5_000, 0.8).await;
        }
    }
    false
}

/// Turn PAST the field of view, spend one leg there, then turn back onto the partner and confirm it.
///
/// The `net_over_sweep` beat: a duet whose camera rests on one body sweeps almost nothing, and a
/// window holding one slow drift reads as a ratio of 1.0 — the "turn away and never come back" shape
/// v5 is full of. `away_deg` exceeds the 102.5 deg horizontal FOV so the away view shares no pixels
/// with the partner view, and away-plus-back fits inside one 4.65 s window (2 x ~110 deg at the
/// configured 72 deg/s is ~3.1 s, plus one 0.7-0.9 s leg).
async fn away_and_back(
    ctx: &mut Ctx,
    partner: Option<usize>,
    fallback: (f64, f64, f64),
    away_deg: f64,
    side_key: WalkDirection,
) {
    let Some(own) = ctx.position() else { return };
    let target = partner
        .and_then(|j| ctx.peer_position(j))
        .map(head_of)
        .unwrap_or(fallback);
    let base_yaw = look_angles(head_of(own), target).0;
    let away = point_at_yaw(head_of(own), base_yaw + away_deg, 24.0);
    let left = ctx.look_until_facing(away, PEER_TOL_DEG, 5_000).await;
    // Strafe, not forward/back: with the camera 110 deg off the partner those keys move the pair
    // together or apart on an unpredictable bearing, while a strafe leaves the gap roughly alone.
    let dwell_ms = vary(ctx, 700.0, 950.0) as u64;
    ctx.hold_visual(side_key, false, dwell_ms.max(MIN_PRESS_MS), 0)
        .await;
    let returned = face_partner(ctx, partner, fallback).await;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-duet-lesson-away-{}-{}", ctx.index(), ctx.elapsed_ms()),
        "Turn off the partner past the field of view, then confirm the return onto it.",
        json!({
            "away_deg": r2(away_deg),
            "exceeds_horizontal_fov": true,
            "left_view": left,
            "return_confirmed": returned,
        }),
    );
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let bot_count = ctx.player_count().max(1);
    let partner = Some(index ^ 1).filter(|&j| j < bot_count);
    let side = if global.is_multiple_of(2) { 1.0 } else { -1.0 };
    // Opposite strafe keys for the WATCHING beat. The performances are already separated in time, so
    // this only hardens the near-clones margin on the framing legs, where both bots are otherwise
    // doing the same small thing.
    let side_key = if side > 0.0 {
        WalkDirection::Left
    } else {
        WalkDirection::Right
    };
    let away_deg = 110.0 + side * 15.0;

    let anchor = ctx.anchor();
    let axis = shared_axis(ctx);
    let mark = (
        anchor.0 + axis.0 * side * MARK_HALF,
        anchor.1 + axis.1 * side * MARK_HALF,
    );

    // The opening shot, before any travel: at `spread_range: 8` the partner is within a dozen blocks
    // at spawn, so the first recorded view already contains the teammate.
    let opening = face_partner(ctx, partner, (anchor.0, 64.0, anchor.1)).await;
    travel_to(ctx, mark, TRAVEL_HOP, 1.5).await;
    let start = ctx.position().unwrap_or((mark.0, 64.0, mark.1));
    let centre = (anchor.0, start.1 + 1.0, anchor.1);
    ctx.record(
        "observe",
        &format!("vis-duet-lesson-setup-{index}"),
        "Take the mark facing the partner; lessons alternate demonstrator and student.",
        json!({
            "global_index": global,
            "role": "shadow_lesson",
            "partner": partner,
            "side": side,
            "mark": {"x": r2(mark.0), "z": r2(mark.1)},
            "opening_shot_confirmed": opening,
        }),
    );

    let mut last_half = i64::MIN;
    while !ctx.target_met() {
        // `phase_at` reads one process-wide epoch, so both bots agree on the half without a barrier.
        // Parity against `global` makes exactly one of them the performer, and the LESSON number (two
        // halves) selects the figure — so the student's half replays what the demonstrator's did.
        let half = ctx.engine().shared.phase_at(HALF_MS);
        let lesson = half.div_euclid(2);
        let performing = (half + global as i64).rem_euclid(2) == 0;
        let figure = (mix(lesson.max(0) as u64) % FIGURES.len() as u64) as usize;
        if half != last_half {
            last_half = half;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-duet-lesson-half-{index}-{half}"),
                if performing {
                    "This side performs the lesson figure while the partner watches."
                } else {
                    "This side watches the partner perform, then repeats it next half."
                },
                json!({
                    "half": half,
                    "lesson": lesson,
                    "performing": performing,
                    "figure": FIGURE_NAMES[figure],
                    // The pairing key: the demonstrator's half and the student's half carry the same
                    // lesson and figure, so the two recordings of one action are joinable offline.
                    "is_imitation": !performing,
                }),
            );
        }

        // Re-acquire the partner before every beat, standing. Wide turns belong here — the body is
        // not translating, so they are not evidence for the camera/travel gate — and this is what
        // guarantees the body is in frame when the step starts.
        face_partner(ctx, partner, centre).await;
        if performing {
            let mut completed = 0usize;
            for &(dir, ms) in FIGURES[figure] {
                if ctx.target_met() {
                    break;
                }
                // Per-step jitter on top of the table so two performances of one figure are never
                // frame-identical even though they are the same figure.
                let jittered = (ms as f64 * vary(ctx, 0.9, 1.15)) as u64;
                if !step(ctx, partner, dir, jittered).await {
                    break;
                }
                completed += 1;
                // Re-centre between steps: the partner drifts on its own leg, and a performance
                // whose audience has slid out of frame is not the shot this scenario exists for.
                face_partner(ctx, partner, centre).await;
            }
            ctx.record(
                TYPE_MOVE,
                &format!("vis-duet-lesson-perform-{index}-{half}"),
                "Perform the lesson figure with the partner held in frame throughout.",
                json!({
                    "half": half,
                    "lesson": lesson,
                    "figure": FIGURE_NAMES[figure],
                    "steps_completed": completed,
                    "steps_total": FIGURES[figure].len(),
                    "is_imitation": !performing,
                }),
            );
        } else {
            // Watching is not standing: the frozen gate fails 20 s of sub-0.5-block motion, and the
            // audience role is where those stretches historically came from. A framing leg is one
            // long press with the view pinned on the performer, so the study shot is uninterrupted.
            let leg_ms = vary(ctx, 800.0, 1_200.0) as u64;
            ctx.hold_visual(side_key, false, leg_ms, 0).await;
            away_and_back(ctx, partner, centre, away_deg, side_key).await;
        }
        // Per-bot pause so the two sides never issue their beats on the same frame.
        sleep_ms(vary(ctx, 200.0, 700.0) as u64).await;
    }
}

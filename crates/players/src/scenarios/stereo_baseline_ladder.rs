//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=true scenery=none far_players=false

use super::*;

/// Half the horizontal field of view (102.5 deg at `fov_deg=70`, 16:9). A teammate is IN FRAME only
/// inside this angle off the optical axis — "nearby" is not "visible", and conflating the two is
/// exactly the defect behind the 22.4% of v5 multiplayer windows whose teammate walks in halfway
/// through (`ops/frustum_audit.py`).
const HALF_FOV_DEG: f64 = 51.25;

/// Most one open-loop press can cover: 1.85 s at ~4.3 blocks/s. The dwell alternates a press with
/// its opposite, so a bot ends where it started — but a press whose partner stalled leaves it a
/// whole excursion off station, and the tightest rung has to survive that.
const MAX_PRESS_BLOCKS: f64 = 8.0;

/// The discrete baselines, in blocks, the pair puts between its two cameras.
///
/// Rungs are >= 8 blocks apart because each transition is split between the two bots (each moves
/// half the change), and a leg under ~4 blocks is a press the key-chatter gate counts as short. The
/// tightest rung is set by the press excursion rather than by taste: one bot a full press off
/// station must still clear [`MIN_PEER_GAP`], which is also why only one of the two dwells along the
/// pair axis in any one step (see `pinned_dwell`).
const RUNGS: [f64; 4] = [13.0, 21.0, 31.0, 43.0];
const _: () = assert!(RUNGS[0] - MAX_PRESS_BLOCKS >= MIN_PEER_GAP);

/// The ladder is climbed AND descended: the pair ends every cycle on the baseline it opened on, so
/// the trajectory is a loop rather than the one-way ray the frustum audit flags (`straight_line_walk`'s
/// 16 blocks of single-direction travel is the reference failure).
const LADDER: [usize; 6] = [0, 1, 2, 3, 2, 1];

/// Distance from the subject to the midpoint of the baseline.
///
/// Sized from the frame, not from taste. The angle at one camera between "look at the subject" and
/// "look at the teammate" is `90 - atan(half_rung / standoff)`: 72 deg at the tightest rung (13) and
/// 43 deg at the widest (43). A camera aimed at the BISECTOR of the two therefore holds both, at
/// every rung, with 36 deg and 21 deg to spare against the 51.25 deg half-frame. Aim straight at the
/// subject instead and the teammate sits ~90 deg off-axis and is never in frame at all — which is
/// what makes a naive lateral stereo pair useless for a multiplayer world model.
const SUBJECT_STANDOFF: f64 = 20.0;

const MATERIAL: &str = "minecraft:stone_bricks";
/// Three blocks is tall enough to stay above ground clutter at 20 blocks and cheap enough that the
/// standing time it costs the builder does not threaten the participation floor.
const SUBJECT_HEIGHT: i64 = 3;

/// How long one rung is observed, before the per-bot term below. Long enough for several pinned
/// presses, short enough that a 120 s episode still climbs and descends the whole ladder.
const RUNG_DWELL_MS: i64 = 7_000;
/// Time spent looking away between rungs. Shorter than the rung dwell: the away view is the part
/// of the window we do NOT want to dominate.
const AWAY_DWELL_MS: i64 = 4_500;
/// Past the 102.5 deg frame, so leaving a rung genuinely un-observes it and the next rung's
/// `look_until_facing` is a real re-acquisition rather than a camera that never left.
const AWAY_DEG: f64 = 130.0;

/// The pair's view axis, shared with no messaging at all: every bot in the episode reads the same
/// `engine().seed`, so a bearing derived from it puts both stations on ONE line. Deriving it from
/// `rng_next` instead — a PER-BOT stream — would leave each bot standing on its own private axis
/// while every QC gate still passed, and the recorded geometry would silently describe something
/// that never happened.
fn axis_rad(ctx: &Ctx) -> f64 {
    // Pair 0 keeps the N=2 geometry; at N=4+ rotate each pair's workspace so paired roles do not
    // land on the same stations while preserving one shared axis inside each pair.
    f64::from(ctx.engine().seed % 360).to_radians() + ctx.index().saturating_div(2) as f64 * 0.83
}

/// Signed shortest angle from `from` to `to`, in degrees.
fn delta_deg(from: f64, to: f64) -> f64 {
    (to - from + 540.0) % 360.0 - 180.0
}

/// The point between the subject and the teammate: the bisector this episode frames.
fn bisector(ctx: &Ctx, peer: Option<usize>, subject: (f64, f64, f64)) -> (f64, f64, f64) {
    let Some(head) = peer.and_then(|j| ctx.peer_position(j)).map(head_of) else {
        return subject;
    };
    (
        (subject.0 + head.0) * 0.5,
        (subject.1 + head.1) * 0.5,
        (subject.2 + head.2) * 0.5,
    )
}

/// MEASURED pair geometry for the event stream, never the plan: a dwell walks both bots several
/// blocks along their sight lines, so the nominal rung is an intention and this is what the footage
/// actually contains. Downstream sample selection ("windows with a 20-30 block baseline", "windows
/// where the teammate is in frame") reads these fields.
fn pair_geometry(
    ctx: &Ctx,
    peer: Option<usize>,
    subject: (f64, f64, f64),
    facing_deg: f64,
) -> Value {
    let Some(own) = ctx.position() else {
        return json!({"peer_visible": false});
    };
    let eye = head_of(own);
    let subject_off = delta_deg(facing_deg, look_angles(eye, subject).0);
    let mut out = json!({
        "subject_distance": r2((subject.0 - own.0).hypot(subject.2 - own.2)),
        "subject_off_axis_deg": r2(subject_off),
        "subject_in_frame": subject_off.abs() <= HALF_FOV_DEG,
        "facing_deg": r2(facing_deg),
        "peer_visible": false,
    });
    let Some(head) = peer.and_then(|j| ctx.peer_position(j)).map(head_of) else {
        return out;
    };
    let baseline = (head.0 - own.0).hypot(head.2 - own.2);
    let peer_off = delta_deg(facing_deg, look_angles(eye, head).0);
    // The convergence angle at the subject — the parallax the two cameras actually deliver. Taken
    // from the two live positions rather than from the rung, because that is the number a sample
    // filter needs to be true.
    let (own_v, peer_v) = (
        (own.0 - subject.0, own.2 - subject.2),
        (head.0 - subject.0, head.2 - subject.2),
    );
    let convergence = (own_v.0 * peer_v.0 + own_v.1 * peer_v.1)
        / (own_v.0.hypot(own_v.1) * peer_v.0.hypot(peer_v.1)).max(1e-6);
    out["peer_visible"] = json!(true);
    out["baseline"] = json!(r2(baseline));
    out["peer_off_axis_deg"] = json!(r2(peer_off));
    out["peer_in_frame"] = json!(peer_off.abs() <= HALF_FOV_DEG);
    out["convergence_deg"] = json!(r2(convergence.clamp(-1.0, 1.0).acos().to_degrees()));
    out
}

/// Cover ground with the camera PINNED on `bearing_deg`, retreating along the sight line and
/// walking back.
///
/// `hold_visual` is the only primitive that does this: it sets `CameraIntent::LookAt` to the
/// current angle and presses ONE movement key for the whole duration. `move_to` sets
/// `CameraIntent::Hold`, which steers the camera onto the travel bearing (`control.rs:762`) and
/// would throw away the framing every record here is about. Three gates also like this shape: one
/// long press is the opposite of key chatter, a press covers 5-8 blocks so the frozen gate never
/// sees a trailing second under 0.5 blocks, and a camera that does not turn while the body moves
/// contributes nothing to the camera/travel-independence denominator.
///
/// Retreat first, return second, so the dwell ends back on the station and the rung the records
/// describe is the rung the pair actually holds.
///
/// `lateral` picks the key pair and alternates per bot per step. That does two jobs: the frame-by-
/// frame key tuples the near-clones gate compares stop being drawn from the same alphabet, and only
/// ONE of the two bots is stepping along the pair's own axis at a time, so the rung can only be
/// closed from one side and [`RUNGS`]`[0] - `[`MAX_PRESS_BLOCKS`] is a real bound.
async fn pinned_dwell(
    ctx: &mut Ctx,
    bearing_deg: f64,
    deadline_ms: i64,
    press_ms: u64,
    lateral: bool,
) {
    let pair = if lateral {
        [WalkDirection::Left, WalkDirection::Right]
    } else {
        [WalkDirection::Backward, WalkDirection::Forward]
    };
    let mut phase = 0usize;
    while !ctx.target_met() && ctx.elapsed_ms() + press_ms as i64 + 600 < deadline_ms {
        let dir = pair[phase % 2];
        phase += 1;
        if ctx.hold_visual(dir, false, press_ms, 0).await {
            ctx.pause().await;
            continue;
        }
        // `false` is the stall guard: the body is leaning on terrain, and grinding more presses
        // there turns every one of them into a short press. Break the footing loose, then re-pin —
        // `shake_loose` looks somewhere random on its way out.
        shake_loose(ctx).await;
        if let Some(p) = ctx.position() {
            ctx.look_default(point_at_yaw(head_of(p), bearing_deg, 24.0))
                .await;
        }
    }
}

/// Two cameras climb and descend a ladder of DISCRETE baselines across one shared subject.
///
/// The revisit problem, both halves: the TRAJECTORY closes because [`LADDER`] is a palindrome — the
/// pair ends each cycle on the baseline it opened on — and the SIGHT LINE closes because every rung
/// is left by a turn past the field of view and re-entered through a `look_until_facing` that
/// CONFIRMS the camera arrived (a plain `look` returns on its deadline, so a wide turn that timed
/// out would be recorded as a return that never happened). Occlusion is whatever the real terrain
/// between the two stations provides; the level type is never overridden.
///
/// What it gives the world model: parallax as a controlled, labelled variable. Each rung is a
/// stationary sample of "the same subject and the same teammate from a known separation", and the
/// event stream carries the measured baseline, the convergence angle at the subject and each
/// camera's off-axis angle to the other — so windows can be selected by parallax magnitude, which
/// is the conditioning signal a multi-view generator needs and a hard-coded two-player corpus
/// cannot vary.
///
/// Distinct from `split_and_reunite`, which separates into REGIONS and then looks at its own
/// subgroup: here the separation is one measured axis, both cameras hold one subject throughout,
/// and neither bot ever loses sight of the other.
pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let peer = Some(index ^ 1).filter(|&j| j < ctx.player_count());
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let axis = axis_rad(ctx);
    let (view, perp) = ((axis.cos(), axis.sin()), (-axis.sin(), axis.cos()));
    // Which end of the baseline this bot holds. Even/odd off the GLOBAL index so the two ends are
    // assigned identically no matter how the swarm is enumerated.
    let side = if global.is_multiple_of(2) { 1.0 } else { -1.0 };
    // Per-bot press length and dwell term. The near-clones gate compares packed key tuples FRAME BY
    // FRAME, so two bots pressing the same key on the same frames is the failure mode (measured
    // 80.2% against an 80% ceiling on a synchronized eval script); derived from `global_index` so
    // the two streams drift apart while the geometry stays exact.
    let press_ms = 1_300 + (global % 3) as u64 * 250;
    let dwell_term = RUNG_DWELL_MS + (global % 2) as i64 * 1_600;

    // Only bot 0 raises the subject. Building is a stretch of standing still (turn to the cell,
    // place, verify) and on the builder that stretch is what pushed `moving frames` under the
    // participation floor in the eval family; the other bot finds the pillar already standing.
    let desired = (
        anchor.0.round() as i64,
        start.1.round() as i64,
        anchor.1.round() as i64,
    );
    let raised = if global == 0 {
        raise_landmark(ctx, desired, SUBJECT_HEIGHT, MATERIAL).await
    } else {
        None
    };
    let subject = raised
        .map(|(_, point)| point)
        .unwrap_or((anchor.0, start.1 + 2.0, anchor.1));
    ctx.record(
        "observe",
        &format!("vis-ladder-setup-{index}"),
        "Resolve the shared baseline axis and the subject the ladder is measured against.",
        json!({
            "global_index": global,
            "role": if side > 0.0 { "baseline_plus" } else { "baseline_minus" },
            "axis_deg": r2(axis.to_degrees()),
            "standoff": SUBJECT_STANDOFF,
            "rungs": RUNGS,
            "subject": {"x": r2(subject.0), "y": r2(subject.1), "z": r2(subject.2)},
            "subject_placed": raised.is_some(),
            "press_ms": press_ms,
        }),
    );
    // Open the two key streams out of phase. Everything after this is per-bot in length, but the
    // first press would otherwise start on the same frame for both.
    sleep_ms(global as u64 * 900).await;

    let mut step = 0usize;
    while !ctx.target_met() {
        let rung_index = LADDER[step % LADDER.len()];
        let rung = RUNGS[rung_index];
        let station = (
            subject.0 + view.0 * SUBJECT_STANDOFF + perp.0 * side * rung * 0.5,
            subject.2 + view.1 * SUBJECT_STANDOFF + perp.1 * side * rung * 0.5,
        );
        travel_to(ctx, station, TRAVEL_HOP, 1.5).await;

        // Frame the bisector, not the subject: see [`SUBJECT_STANDOFF`] for why that is what keeps
        // the teammate inside the frame at every rung.
        let focus = bisector(ctx, peer, subject);
        let faced = ctx.look_until_facing(focus, 6.0, 5_000).await;
        let facing = ctx
            .position()
            .map(|p| look_angles(head_of(p), focus).0)
            .unwrap_or(0.0);
        let lateral = (global + step).is_multiple_of(2);
        let mut extras = pair_geometry(ctx, peer, subject, facing);
        extras["dwell_lateral"] = json!(lateral);
        extras["rung_index"] = json!(rung_index);
        extras["rung_nominal"] = json!(rung);
        extras["step"] = json!(step);
        extras["climbing"] = json!(step % LADDER.len() < 3);
        extras["is_return"] = json!(step > 0);
        extras["faced"] = json!(faced);
        ctx.record(
            "observe",
            &format!("vis-ladder-rung-{index}-{step}"),
            &format!("Hold rung {rung_index} ({rung} blocks nominal) on the shared subject."),
            extras,
        );
        pinned_dwell(
            ctx,
            facing,
            ctx.elapsed_ms() + dwell_term,
            press_ms,
            lateral,
        )
        .await;
        if ctx.target_met() {
            break;
        }

        // Leave the rung past the frame edge, alternating sign so the episode is not one continuous
        // rotation — the 0.80 net-turn / sweep ratio v5 measures is that shape, and a window with no
        // reversal in it contains no self-check at all.
        let away = facing + AWAY_DEG * if step.is_multiple_of(2) { 1.0 } else { -1.0 };
        // CONFIRM the turn, do not merely ask for it. `look` returns when its settle window expires,
        // and a 130 deg turn at 72 deg/s needs ~1.8 s — longer than the default settle — after which
        // `hold_visual` would pin the camera wherever it happened to be, leaving an "away" view that
        // still overlaps the rung and a record claiming a turn that never happened.
        let left = match ctx.position() {
            Some(p) => {
                ctx.look_until_facing(point_at_yaw(head_of(p), away, 24.0), 6.0, 5_000)
                    .await
            }
            None => false,
        };
        ctx.record(
            "observe",
            &format!("vis-ladder-away-{index}-{step}"),
            "Turn off the rung past the field of view; the next rung is a confirmed return.",
            json!({
                "step": step,
                "turn_away_deg": AWAY_DEG,
                "away_deg": r2(away),
                "reached_away": left,
                "exceeds_horizontal_fov": AWAY_DEG > HALF_FOV_DEG,
            }),
        );
        pinned_dwell(
            ctx,
            away,
            ctx.elapsed_ms() + AWAY_DWELL_MS,
            press_ms,
            lateral,
        )
        .await;
        step += 1;
    }
}

//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=false scenery=none far_players=false

use super::*;

/// Half the horizontal field of view (102.5 deg at `fov_deg=70`, 16:9).
const HALF_FOV_DEG: f64 = 51.25;

/// Half the baseline, in blocks: each camera sits this far from the anchor along the shared axis.
/// Set by [`MAX_CLOSURE`], not by taste.
const HALF_GAP: f64 = 7.0;

/// Angle between the pair's baseline and the shared optical axis.
///
/// This is the number the whole episode turns on. Both cameras hold the SAME world bearing — that is
/// what makes the two POVs a parallel pair — and the teammate lies along the baseline, so one bot
/// sees it at `LOOK_OFFSET_DEG` off-axis and the other at `180 - LOOK_OFFSET_DEG`. At 40 deg the
/// first is inside the 51.25 deg half-frame with margin for the camera quantizer. Aim the pair down
/// the baseline's normal instead — the naive "parallel stereo rig" — and the teammate sits at exactly
/// 90 deg and is never in frame at all, which is what makes that arrangement useless to a
/// multiplayer world model.
const LOOK_OFFSET_DEG: f64 = 40.0;
const _: () = assert!(LOOK_OFFSET_DEG < HALF_FOV_DEG - 5.0);

/// Most the gap can close while both bots are mid-press, and therefore what sizes [`HALF_GAP`].
///
/// Both bots strafe along the same line, so the gap moves only when their L/R phases disagree — and
/// they DO disagree, by design: the per-bot press lengths below exist to drift the two key streams
/// apart. So the two-sided case is the real one, one bot on Left while the other is on Right. Each
/// press is at most ~1.45 s at ~4.3 blocks/s and the strafe runs 50 deg off the baseline (it is
/// perpendicular to the optical axis, which is 40 deg off the baseline), so `cos 50 deg = 0.64` of
/// each closes the gap: `(1.25 + 1.45) * 4.3 * 0.64 = 7.4`. Each bot alternates a press with its
/// opposite, so this is an excursion around its own slot rather than drift that accumulates.
const MAX_CLOSURE: f64 = 7.4;
const _: () = assert!(2.0 * HALF_GAP - MAX_CLOSURE >= MIN_PEER_GAP);

/// Below this the planner walks the bot back to its slot instead of pressing again. This is the
/// RECOVERY path for drift the excursion argument does not cover — a stalled press, or a
/// `shake_loose` hop that threw the body several blocks — not a second hard bound; the bound is the
/// station geometry above.
const CLOSE_GUARD: f64 = 9.0;
const _: () = assert!(CLOSE_GUARD >= MIN_PEER_GAP);

/// Wall clock one heading gets before the pair flips the offset to the other side. Read off
/// `elapsed_ms`, which starts at the shared `go` handshake for every bot in the process, so both
/// bots flip on the same beat with nothing exchanged.
const ROUND_MS: i64 = 18_000;

/// Past the frame edge, so the leader's shoulder check genuinely leaves the shared view and comes
/// back to it — the reversal v5 windows almost never contain (net-turn / sweep ratio 0.80).
const SHOULDER_DEG: f64 = 150.0;

/// The pair's baseline direction. Off `engine().seed`, which every bot in the process reads
/// identically; `rng_next` is a PER-BOT stream and would have put the two bots on two different
/// lines while every QC gate still passed and the recorded geometry described something that never
/// happened.
fn axis_rad(ctx: &Ctx) -> f64 {
    // Pair 0 keeps the N=2 geometry; at N=4+ rotate each pair's workspace so paired roles do not
    // land on the same stations while preserving one shared axis inside each pair.
    f64::from(ctx.engine().seed % 360).to_radians() + ctx.index().saturating_div(2) as f64 * 0.83
}

/// Signed shortest angle from `from` to `to`, in degrees.
fn delta_deg(from: f64, to: f64) -> f64 {
    (to - from + 540.0) % 360.0 - 180.0
}

/// A distant world point in math-convention direction `dir` from the eye. Every look API takes a
/// POINT, and going through one keeps this file in a single angle convention instead of converting
/// into notchian yaw and back, which is how a 90 deg sign error hides.
fn far_point(eye: (f64, f64, f64), dir: (f64, f64)) -> (f64, f64, f64) {
    (eye.0 + dir.0 * 64.0, eye.1, eye.2 + dir.1 * 64.0)
}

/// Notchian yaw of the shared bearing from where this bot stands.
fn bearing_to(ctx: &Ctx, dir: (f64, f64), fallback: f64) -> f64 {
    ctx.position()
        .map(|p| {
            let eye = head_of(p);
            look_angles(eye, far_point(eye, dir)).0
        })
        .unwrap_or(fallback)
}

/// MEASURED pair geometry: the baseline is the quantity that varies here, and it varies by the
/// stagger between two presses rather than by design, so it has to be read off live positions.
fn pair_geometry(
    ctx: &Ctx,
    peer: Option<usize>,
    axis: (f64, f64),
    anchor: (f64, f64),
    facing_deg: f64,
) -> Value {
    let Some(own) = ctx.position() else {
        return json!({"peer_visible": false});
    };
    let mut out = json!({
        "peer_visible": false,
        // Signed position along the baseline, so a reviewer can watch the two bodies slide.
        "axis_offset": r2((own.0 - anchor.0) * axis.0 + (own.2 - anchor.1) * axis.1),
        "facing_deg": r2(facing_deg),
        "parallel_views": true,
    });
    let Some(head) = peer.and_then(|j| ctx.peer_position(j)).map(head_of) else {
        return out;
    };
    let (dx, dz) = (head.0 - own.0, head.2 - own.2);
    let off = delta_deg(facing_deg, look_angles(head_of(own), head).0);
    out["peer_visible"] = json!(true);
    out["baseline"] = json!(r2(dx.hypot(dz)));
    // The baseline split into the part on the shared axis and the part the pair has drifted off it:
    // with both cameras on one bearing, the first is the effective stereo baseline.
    out["baseline_along_axis"] = json!(r2(dx * axis.0 + dz * axis.1));
    out["baseline_off_axis"] = json!(r2(-dx * axis.1 + dz * axis.0));
    out["peer_off_axis_deg"] = json!(r2(off));
    out["peer_in_frame"] = json!(off.abs() <= HALF_FOV_DEG);
    out
}

/// Slide sideways with the camera PINNED on `bearing_deg`: one long strafe press, then one back.
///
/// `hold_visual` is the only primitive that does this — it pins `CameraIntent::LookAt` at the current
/// angle and holds one movement key, where `move_to` sets `CameraIntent::Hold`, which steers the
/// camera onto the travel bearing (`control.rs:762`) and would destroy the parallel-view property
/// the episode is built on. A strafe under a fixed camera is also the cleanest lateral tracking shot
/// available, which is exactly the signal a multi-view generator wants, and one long press is the
/// opposite of key chatter.
///
/// Left then right returns each bot to its own slot, so the trajectory is a loop and the gap cannot
/// accumulate drift; the guard covers the transient while one bot is mid-press and the other is not.
async fn strafe_dwell(
    ctx: &mut Ctx,
    bearing_deg: f64,
    slot: (f64, f64),
    peer: Option<usize>,
    deadline_ms: i64,
    press_ms: u64,
) {
    let mut phase = 0usize;
    while !ctx.target_met() && ctx.elapsed_ms() + press_ms as i64 + 900 < deadline_ms {
        let close = match (ctx.position(), peer.and_then(|j| ctx.peer_position(j))) {
            (Some(own), Some(p)) => (p.0 - own.0).hypot(p.2 - own.2) < CLOSE_GUARD,
            _ => false,
        };
        if close {
            // Too tight for another open-loop press: hand the footing back to the planner, which
            // routes around the teammate's body, then re-pin the shared bearing.
            ctx.move_to(slot.0, slot.1, 6_000, 1.5).await;
            if let Some(p) = ctx.position() {
                ctx.look_default(point_at_yaw(head_of(p), bearing_deg, 24.0))
                    .await;
            }
            continue;
        }
        let dir = if phase.is_multiple_of(2) {
            WalkDirection::Left
        } else {
            WalkDirection::Right
        };
        phase += 1;
        if ctx.hold_visual(dir, false, press_ms, 0).await {
            ctx.pause().await;
            continue;
        }
        // A refused press is the stall guard: the body is against terrain, and grinding more presses
        // there turns them all into the short ones the chatter gate counts.
        shake_loose(ctx).await;
        if let Some(p) = ctx.position() {
            ctx.look_default(point_at_yaw(head_of(p), bearing_deg, 24.0))
                .await;
        }
    }
}

/// Two cameras hold ONE shared bearing a fixed baseline apart and dolly sideways together, so the
/// two POVs are as correlated as two POVs get.
///
/// The revisit problem: the TRAJECTORY closes because every excursion is a strafe out and a strafe
/// back to the same slot, and the SIGHT LINE closes twice over — the shared bearing leans to the
/// other side of the baseline each round and back again, and the bot that cannot see its teammate
/// spends one beat per round turned 150 deg round to it before confirming its way back onto the
/// shared bearing with
/// `look_until_facing` (a plain `look` returns on its deadline, so a wide turn that timed out would
/// be recorded as a return that never happened). Occlusion is whatever the terrain the pair dollies
/// past puts between them; the level type is never overridden, and on a Superflat plain this episode
/// would be worthless.
///
/// What it gives the world model: the strongest cross-view consistency material we can record. Two
/// cameras on one bearing, a known baseline apart, translating in parallel — the classic stereo rig —
/// so a generation for one POV is geometrically pinned by the other with no ground truth needed
/// (the GT-free self-consistency class). The event stream carries the baseline decomposed into its
/// on-axis and off-axis parts, so windows can be selected by effective stereo baseline.
///
/// Distinct from `mirror_dance`, a symmetric partner choreography of figures performed face to face,
/// and from `formation_walk`, which translates a formation toward waypoints: here the pair holds one
/// heading it never travels along, and the parallelism of the two views is itself the product.
pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let peer = Some(index ^ 1).filter(|&j| j < ctx.player_count());
    let anchor = ctx.anchor();
    let axis_angle = axis_rad(ctx);
    let axis = (axis_angle.cos(), axis_angle.sin());
    // Which end of the baseline this bot holds, off the GLOBAL index so the two ends are assigned the
    // same way however the swarm is enumerated. The bot the shared bearing leans away from is the one
    // holding the other in frame; the other is the leader and owes a shoulder check.
    let lean = if global.is_multiple_of(2) { 1.0 } else { -1.0 };
    let slot = (
        anchor.0 - axis.0 * HALF_GAP * lean,
        anchor.1 - axis.1 * HALF_GAP * lean,
    );
    // Per-bot press length, capped by the 1.85 s [`MAX_CLOSURE`] budget. The near-clones gate
    // compares packed key tuples FRAME BY FRAME and this is the one scenario where both bots
    // genuinely press the same keys, so the whole defence is that they press them for different
    // lengths, out of phase, and that the leader spends a beat each round doing something else.
    let press_ms = 1_250 + (global % 4) as u64 * 200;
    ctx.record(
        "observe",
        &format!("vis-rail-setup-{index}"),
        "Resolve the shared baseline, this camera's slot and the shared optical axis.",
        json!({
            "global_index": global,
            "axis_deg": r2(axis_angle.to_degrees()),
            "slot": {"x": r2(slot.0), "z": r2(slot.1)},
            "half_gap": HALF_GAP,
            "look_offset_deg": LOOK_OFFSET_DEG,
            "press_ms": press_ms,
        }),
    );
    sleep_ms(global as u64 * 900).await;

    while !ctx.target_met() {
        let round = (ctx.elapsed_ms() / ROUND_MS).max(0);
        let deadline = (round + 1) * ROUND_MS;
        // The shared bearing leans to one side of the baseline, then to the other. Flipping the
        // bearing (rather than having the two bots trade slots) swaps who holds the teammate in frame
        // WITHOUT two bodies crossing on one line, and the flip is itself an 80 deg turn away and
        // back that the round's windows can be checked against.
        let lean_now = if round % 2 == 0 { 1.0 } else { -1.0 };
        let look_rad = axis_angle + LOOK_OFFSET_DEG.to_radians() * lean_now;
        let look_dir = (look_rad.cos(), look_rad.sin());
        let watcher = lean * lean_now < 0.0;

        travel_to(ctx, slot, TRAVEL_HOP, 1.5).await;
        let Some(own) = ctx.position() else {
            ctx.pause().await;
            continue;
        };
        let faced = ctx
            .look_until_facing(far_point(head_of(own), look_dir), 6.0, 5_000)
            .await;
        let facing = bearing_to(ctx, look_dir, 0.0);
        let mut extras = pair_geometry(ctx, peer, axis, anchor, facing);
        extras["round"] = json!(round);
        extras["role"] = json!(if watcher { "watcher" } else { "leader" });
        extras["lean"] = json!(lean_now);
        extras["faced"] = json!(faced);
        ctx.record(
            "observe",
            &format!("vis-rail-heading-{index}-{round}"),
            "Both cameras take the shared bearing; the gap between them is the stereo baseline.",
            extras,
        );

        // The leader owes the shoulder check: on this heading its teammate is behind the frame edge,
        // so it spends the first beat of the round turned round to it and then confirms its way back.
        // The watcher spends that beat dollying instead — two different key streams on the same
        // frames, which is the other half of the near-clones defence.
        if !watcher {
            let head = peer.and_then(|j| ctx.peer_position(j)).map(head_of);
            let shoulder = facing + SHOULDER_DEG * lean_now;
            let reached = match head {
                Some(head) => ctx.look_until_facing(head, 8.0, 4_000).await,
                None => {
                    if let Some(p) = ctx.position() {
                        ctx.look_default(point_at_yaw(head_of(p), shoulder, 24.0))
                            .await;
                    }
                    false
                }
            };
            let bearing = ctx
                .position()
                .zip(head)
                .map(|(p, head)| look_angles(head_of(p), head).0)
                .unwrap_or(shoulder);
            let mut extras = pair_geometry(ctx, peer, axis, anchor, bearing);
            extras["round"] = json!(round);
            extras["shoulder_check"] = json!(true);
            extras["turn_deg"] = json!(SHOULDER_DEG);
            extras["exceeds_horizontal_fov"] = json!(SHOULDER_DEG > HALF_FOV_DEG);
            extras["reached_peer"] = json!(reached);
            ctx.record(
                "observe",
                &format!("vis-rail-shoulder-{index}-{round}"),
                "Leader turns past the frame edge to check on the teammate, then turns back.",
                extras,
            );
            let until = (ctx.elapsed_ms() + press_ms as i64 * 2 + 900).min(deadline);
            strafe_dwell(ctx, bearing, slot, peer, until, press_ms).await;
            if let Some(p) = ctx.position() {
                ctx.look_until_facing(far_point(head_of(p), look_dir), 6.0, 5_000)
                    .await;
            }
        }

        let bearing = bearing_to(ctx, look_dir, facing);
        strafe_dwell(ctx, bearing, slot, peer, deadline, press_ms).await;
    }
}

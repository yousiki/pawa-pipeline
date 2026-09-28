//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=true scenery=none far_players=false

use super::*;

/// A stepped baseline that opens, closes, and opens again around one shared landmark.
///
/// The revisit problem is covered by both loops: the pair walks to each station on a palindromic
/// baseline schedule, then each station uses a confirmed look at the same landmark before the next
/// move. The trajectory therefore returns to its opening geometry, while the sight line is
/// re-acquired after every baseline change. The landmark is a small script-built subject; terrain
/// remains the real generated world, so hills and trees can still occlude one member of a pair. The
/// measured baseline, subject distance, relative bearing, and co-visibility flag make the stereo
/// condition selectable.
///
/// This is intentionally not `stereo_baseline_ladder`: both bodies move symmetrically here, but the
/// baseline is pulsed through the same narrow range repeatedly rather than climbed as a one-way rung
/// sequence. It also differs from `stereo_parallel_rail`, whose baseline is fixed while gaze leans.
const MATERIAL: &str = "minecraft:stone_bricks";
const SUBJECT_HEIGHT: i64 = 3;
const HALF_FOV_DEG: f64 = 51.25;
/// Eight blocks is large enough to survive one open-loop press and still leaves the pair separated by
/// more than `MIN_PEER_GAP` after a transient inward excursion.
const MIN_BASELINE: f64 = 10.0;
const MAX_BASELINE: f64 = 22.0;
const SUBJECT_STANDOFF: f64 = 13.0;
const STEP_MS: i64 = 8_000;
const PRESS_MS: u64 = 1_350;
const SCHEDULE: [f64; 8] = [
    MIN_BASELINE,
    16.0,
    MAX_BASELINE,
    16.0,
    MIN_BASELINE,
    16.0,
    MAX_BASELINE,
    16.0,
];
const _: () = assert!(MIN_BASELINE - 2.0 >= MIN_PEER_GAP);

fn axis_rad(ctx: &Ctx) -> f64 {
    // Pair 0 keeps the N=2 geometry; at N=4+ rotate each pair's workspace so paired roles do not
    // land on the same stations while preserving one shared axis inside each pair.
    f64::from(ctx.engine().seed % 360).to_radians() + ctx.index().saturating_div(2) as f64 * 0.83
}

fn delta_deg(from: f64, to: f64) -> f64 {
    (to - from + 540.0) % 360.0 - 180.0
}

fn pair_geometry(ctx: &Ctx, peer: Option<usize>, subject: (f64, f64, f64), facing: f64) -> Value {
    let Some(own) = ctx.position() else {
        return json!({"peer_visible": false});
    };
    let eye = head_of(own);
    let subject_bearing = look_angles(eye, subject).0;
    let mut out = json!({
        "peer_visible": false,
        "baseline": null,
        "subject_distance": r2((subject.0 - own.0).hypot(subject.2 - own.2)),
        "relative_bearing_deg": r2(delta_deg(facing, subject_bearing)),
        "facing_deg": r2(facing),
    });
    let Some(head) = peer.and_then(|j| ctx.peer_position(j)).map(head_of) else {
        return out;
    };
    let peer_bearing = look_angles(eye, head).0;
    let peer_off = delta_deg(facing, peer_bearing);
    let subject_off = delta_deg(facing, subject_bearing);
    out["peer_visible"] = json!(true);
    out["baseline"] = json!(r2((head.0 - own.0).hypot(head.2 - own.2)));
    out["peer_relative_bearing_deg"] = json!(r2(peer_off));
    out["subject_in_frame"] = json!(subject_off.abs() <= HALF_FOV_DEG);
    out["peer_in_frame"] = json!(peer_off.abs() <= HALF_FOV_DEG);
    out["covisibility"] =
        json!(subject_off.abs() <= HALF_FOV_DEG && peer_off.abs() <= HALF_FOV_DEG);
    out
}

/// Alternate long lateral presses under a pinned camera. One bot's phase is offset by its global
/// index, so the symmetric station plan does not become a frame-identical near-clone stream.
async fn dwell(ctx: &mut Ctx, facing: f64, deadline: i64, phase: usize) {
    let pair = if phase.is_multiple_of(2) {
        [WalkDirection::Left, WalkDirection::Right]
    } else {
        [WalkDirection::Right, WalkDirection::Left]
    };
    let mut i = 0usize;
    while !ctx.target_met() && ctx.elapsed_ms() + PRESS_MS as i64 + 700 < deadline {
        if ctx.hold_visual(pair[i % 2], false, PRESS_MS, 0).await {
            ctx.pause().await;
        } else {
            shake_loose(ctx).await;
            if let Some(p) = ctx.position() {
                ctx.look_default(point_at_yaw(head_of(p), facing, 24.0))
                    .await;
            }
        }
        i += 1;
    }
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let peer = Some(index ^ 1).filter(|&j| j < ctx.player_count());
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let axis = axis_rad(ctx);
    let (ux, uz) = (axis.cos(), axis.sin());
    let side = if global.is_multiple_of(2) { 1.0 } else { -1.0 };
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
        .map(|(_, p)| p)
        .unwrap_or((anchor.0, start.1 + 2.0, anchor.1));
    ctx.record(
        TYPE_LOOK,
        &format!("vis-stereo-baseline-setup-{index}"),
        "Resolve the shared subject and the pulsed baseline axis.",
        json!({"global_index": global, "axis_deg": r2(axis.to_degrees()),
            "schedule": SCHEDULE, "subject_standoff": SUBJECT_STANDOFF,
            "subject": {"x": r2(subject.0), "y": r2(subject.1), "z": r2(subject.2)},
            "subject_placed": raised.is_some()}),
    );
    sleep_ms(global as u64 * 850).await;

    let mut step = 0usize;
    while !ctx.target_met() {
        let baseline_nominal = SCHEDULE[step % SCHEDULE.len()];
        let station = (
            subject.0 - ux * SUBJECT_STANDOFF + ux * side * baseline_nominal * 0.5,
            subject.2 - uz * SUBJECT_STANDOFF + uz * side * baseline_nominal * 0.5,
        );
        travel_to(ctx, station, TRAVEL_HOP, 1.5).await;
        let faced = ctx.look_until_facing(subject, 6.0, 5_000).await;
        let facing = ctx
            .position()
            .map(|p| look_angles(head_of(p), subject).0)
            .unwrap_or(0.0);
        let mut geometry = pair_geometry(ctx, peer, subject, facing);
        geometry["step"] = json!(step);
        geometry["baseline_nominal"] = json!(baseline_nominal);
        geometry["baseline_opening"] = json!(step == 0);
        geometry["baseline_return"] = json!(step > 0 && baseline_nominal == SCHEDULE[0]);
        geometry["faced"] = json!(faced);
        ctx.record(
            TYPE_LOOK,
            &format!("vis-stereo-baseline-hold-{index}-{step}"),
            "Hold the current baseline pulse on the shared landmark.",
            geometry,
        );
        dwell(ctx, facing, ctx.elapsed_ms() + STEP_MS, global + step).await;
        step += 1;
    }
}

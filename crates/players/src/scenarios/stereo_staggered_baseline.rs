//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=true scenery=none far_players=false

use super::*;

/// Climb and descend a baseline staircase by moving only one camera at a time.
///
/// The revisit problem's trajectory loop is encoded directly in two offset schedules: the pair
/// opens at a 10-block baseline, widens by one four-block leg per beat to 26, and retraces to 10.
/// Only one bot changes station on any beat, so every parallax step has one stable reference
/// camera. The sight line loop re-acquires the same landmark after each step with
/// `look_until_facing`. Terrain remains the generated world and may interrupt either ray; records
/// carry measured baseline, distance, relative bearing, co-visibility and which side advanced. If a
/// pathfinder detour spoils a nominal rung, the live geometry remains the authority rather than the
/// requested offset.
///
/// This is not `stereo_baseline_ladder`, where both cameras move symmetrically between rungs. It is not
/// `stereo_arc_sweep` either: the moving camera follows one straight baseline axis, not an azimuth arc,
/// and the other camera is an explicit per-step reference.
const MATERIAL: &str = "minecraft:stone_bricks";
const SUBJECT_HEIGHT: i64 = 3;
const HALF_FOV_DEG: f64 = 51.25;
const SUBJECT_STANDOFF: f64 = 14.0;
const STEP_MS: i64 = 7_500;
const PRESS_MS: u64 = 1_350;
const LEFT: [f64; 9] = [5.0, 5.0, 9.0, 9.0, 13.0, 13.0, 9.0, 9.0, 5.0];
const RIGHT: [f64; 9] = [5.0, 9.0, 9.0, 13.0, 13.0, 9.0, 9.0, 5.0, 5.0];
const _: () = assert!(LEFT[0] + RIGHT[0] >= MIN_PEER_GAP);

fn axis_rad(ctx: &Ctx) -> f64 {
    // Pair 0 keeps the N=2 geometry; at N=4+ rotate each pair's workspace so paired roles do not
    // land on the same stations while preserving one shared axis inside each pair.
    f64::from(ctx.engine().seed % 360).to_radians() + ctx.index().saturating_div(2) as f64 * 0.83
}

fn delta_deg(from: f64, to: f64) -> f64 {
    (to - from + 540.0) % 360.0 - 180.0
}

fn geometry(ctx: &Ctx, peer: Option<usize>, subject: (f64, f64, f64), facing: f64) -> Value {
    let Some(own) = ctx.position() else {
        return json!({"peer_visible": false});
    };
    let eye = head_of(own);
    let sb = look_angles(eye, subject).0;
    let mut out = json!({
        "peer_visible": false,
        "subject_distance": r2((subject.0 - own.0).hypot(subject.2 - own.2)),
        "relative_bearing_deg": r2(delta_deg(facing, sb)),
        "facing_deg": r2(facing),
    });
    let Some(peer_pos) = peer.and_then(|j| ctx.peer_position(j)) else {
        return out;
    };
    let peer_head = head_of(peer_pos);
    let so = delta_deg(facing, sb);
    let po = delta_deg(facing, look_angles(eye, peer_head).0);
    out["peer_visible"] = json!(true);
    out["baseline"] = json!(r2((peer_pos.0 - own.0).hypot(peer_pos.2 - own.2)));
    out["height_baseline"] = json!(r2(peer_pos.1 - own.1));
    out["peer_relative_bearing_deg"] = json!(r2(po));
    out["subject_in_frame"] = json!(so.abs() <= HALF_FOV_DEG);
    out["peer_in_frame"] = json!(po.abs() <= HALF_FOV_DEG);
    out["covisibility"] = json!(so.abs() <= HALF_FOV_DEG && po.abs() <= HALF_FOV_DEG);
    out
}

async fn shuffle(ctx: &mut Ctx, facing: f64, deadline: i64, lateral: bool) {
    let dirs = if lateral {
        [WalkDirection::Left, WalkDirection::Right]
    } else {
        [WalkDirection::Backward, WalkDirection::Forward]
    };
    let mut n = 0usize;
    while !ctx.target_met() && ctx.elapsed_ms() + PRESS_MS as i64 + 700 < deadline {
        if ctx.hold_visual(dirs[n % 2], false, PRESS_MS, 0).await {
            ctx.pause().await;
        } else {
            shake_loose(ctx).await;
            if let Some(p) = ctx.position() {
                ctx.look_default(point_at_yaw(head_of(p), facing, 24.0))
                    .await;
            }
        }
        n += 1;
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
    let (px, pz) = (-uz, ux);
    let left_side = global.is_multiple_of(2);
    let side = if left_side { -1.0 } else { 1.0 };
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
        &format!("vis-stereo-stagger-setup-{index}"),
        "Resolve the one-camera-at-a-time baseline staircase.",
        json!({"global_index": global, "axis_deg": r2(axis.to_degrees()),
            "left_offsets": LEFT, "right_offsets": RIGHT, "subject_standoff": SUBJECT_STANDOFF,
            "role": if left_side { "left_camera" } else { "right_camera" },
            "subject": {"x": r2(subject.0), "y": r2(subject.1), "z": r2(subject.2)},
            "subject_placed": raised.is_some()}),
    );
    sleep_ms(global as u64 * 900).await;

    let mut step = 0usize;
    while !ctx.target_met() {
        let rung = step % LEFT.len();
        let offset = if left_side { LEFT[rung] } else { RIGHT[rung] };
        let station = (
            subject.0 - ux * SUBJECT_STANDOFF + px * side * offset,
            subject.2 - uz * SUBJECT_STANDOFF + pz * side * offset,
        );
        let previous = if rung == 0 {
            if left_side {
                LEFT[LEFT.len() - 1]
            } else {
                RIGHT[RIGHT.len() - 1]
            }
        } else if left_side {
            LEFT[rung - 1]
        } else {
            RIGHT[rung - 1]
        };
        let advanced = (offset - previous).abs() > 0.1;
        travel_to(ctx, station, TRAVEL_HOP, 1.5).await;
        let faced = ctx.look_until_facing(subject, 6.0, 5_000).await;
        let facing = ctx
            .position()
            .map(|p| look_angles(head_of(p), subject).0)
            .unwrap_or(0.0);
        let mut data = geometry(ctx, peer, subject, facing);
        data["step"] = json!(step);
        data["rung"] = json!(rung);
        data["offset_nominal"] = json!(offset);
        data["baseline_nominal"] = json!(LEFT[rung] + RIGHT[rung]);
        data["camera_advanced"] = json!(advanced);
        data["stable_reference"] = json!(!advanced);
        data["returned_opening_baseline"] = json!(step > 0 && rung == 0);
        data["faced"] = json!(faced);
        ctx.record(
            TYPE_LOOK,
            &format!("vis-stereo-stagger-rung-{index}-{step}"),
            "Hold the current rung with one camera stable and one newly advanced.",
            data,
        );
        shuffle(
            ctx,
            facing,
            ctx.elapsed_ms() + STEP_MS,
            (global + step).is_multiple_of(2),
        )
        .await;
        step += 1;
    }
}

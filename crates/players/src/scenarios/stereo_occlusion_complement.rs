//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=true scenery=none far_players=false

use super::*;

/// Pair a blocked camera with a lateral peek camera around one shared landmark.
///
/// The revisit problem's occlusion face is complementary rather than merely repeated: at one
/// station a short real wall blocks the landmark, while the partner's offset station sees around
/// that same wall. The two bots exchange those stations on the next beat, so every hidden view has
/// a contemporaneous visible view to supervise it. The trajectory is a station swap followed by a
/// swap back; the gaze loop re-acquires the landmark with `look_until_facing` after each move.
/// Natural terrain remains untouched except for the labelled wall and landmark. Records include a
/// line-of-sight result, baseline, subject distance, relative bearing and co-visibility, with
/// failures recorded rather than claimed as successful occlusion.
///
/// This differs from `occl_wall_peekaboo`, which is a single-camera peek cycle. It is also unlike
/// `stereo_inline_depth_relay`: the baseline is lateral and the primary signal is complementary
/// visibility, not scale along one ray.
const MATERIAL: &str = "minecraft:stone_bricks";
const SUBJECT_HEIGHT: i64 = 3;
const WALL_HEIGHT: i64 = 2;
const HALF_FOV_DEG: f64 = 51.25;
const SUBJECT_RANGE: f64 = 8.0;
const WALL_RANGE: f64 = 3.0;
const CAMERA_RANGE: f64 = 8.0;
const PEEK_OFFSET: f64 = 5.0;
const HOLD_MS: i64 = 8_000;
const PRESS_MS: u64 = 1_350;
const WALL_LANES: [i64; 3] = [-1, 0, 1];

fn axis_rad(ctx: &Ctx) -> f64 {
    // Pair 0 keeps the N=2 geometry; at N=4+ rotate each pair's workspace so paired roles do not
    // land on the same stations while preserving one shared axis inside each pair.
    f64::from(ctx.engine().seed % 360).to_radians() + ctx.index().saturating_div(2) as f64 * 0.83
}

fn delta_deg(from: f64, to: f64) -> f64 {
    (to - from + 540.0) % 360.0 - 180.0
}

fn los_blocked(ctx: &Ctx, eye: (f64, f64, f64), target: (f64, f64, f64)) -> bool {
    let (dx, dy, dz) = (target.0 - eye.0, target.1 - eye.1, target.2 - eye.2);
    let span = (dx * dx + dy * dy + dz * dz).sqrt();
    let steps = (span * 2.0).ceil() as i32;
    (1..steps).any(|step| {
        let t = step as f64 / steps as f64;
        block_is_safe_support(
            ctx,
            BlockPos::new(
                (eye.0 + dx * t).floor() as i32,
                (eye.1 + dy * t).floor() as i32,
                (eye.2 + dz * t).floor() as i32,
            ),
        )
    })
}

fn geometry(ctx: &Ctx, peer: Option<usize>, subject: (f64, f64, f64), facing: f64) -> Value {
    let Some(own) = ctx.position() else {
        return json!({"peer_visible": false});
    };
    let eye = head_of(own);
    let sb = look_angles(eye, subject).0;
    let own_blocked = los_blocked(ctx, eye, subject);
    let mut out = json!({
        "peer_visible": false,
        "subject_distance": r2((subject.0 - own.0).hypot(subject.2 - own.2)),
        "relative_bearing_deg": r2(delta_deg(facing, sb)),
        "facing_deg": r2(facing),
        "occluded": own_blocked,
    });
    let Some(peer_pos) = peer.and_then(|j| ctx.peer_position(j)) else {
        return out;
    };
    let peer_eye = head_of(peer_pos);
    let so = delta_deg(facing, sb);
    let po = delta_deg(facing, look_angles(eye, peer_eye).0);
    let peer_blocked = los_blocked(ctx, peer_eye, subject);
    out["peer_visible"] = json!(true);
    out["baseline"] = json!(r2((peer_pos.0 - own.0).hypot(peer_pos.2 - own.2)));
    out["height_baseline"] = json!(r2(peer_pos.1 - own.1));
    out["peer_relative_bearing_deg"] = json!(r2(po));
    out["subject_in_frame"] = json!(so.abs() <= HALF_FOV_DEG);
    out["peer_in_frame"] = json!(po.abs() <= HALF_FOV_DEG);
    out["peer_occluded"] = json!(peer_blocked);
    out["covisibility"] = json!(so.abs() <= HALF_FOV_DEG && po.abs() <= HALF_FOV_DEG);
    out["occlusion_complement"] = json!(own_blocked != peer_blocked);
    out
}

async fn shuffle(ctx: &mut Ctx, facing: f64, deadline: i64, phase: usize) {
    let dirs = if phase.is_multiple_of(2) {
        [WalkDirection::Left, WalkDirection::Right]
    } else {
        [WalkDirection::Right, WalkDirection::Left]
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
    let desired_subject = (
        (anchor.0 + ux * SUBJECT_RANGE).round() as i64,
        start.1.round() as i64,
        (anchor.1 + uz * SUBJECT_RANGE).round() as i64,
    );
    let raised_subject = if global == 0 {
        raise_landmark(ctx, desired_subject, SUBJECT_HEIGHT, MATERIAL).await
    } else {
        None
    };
    let subject = raised_subject.map(|(_, p)| p).unwrap_or((
        desired_subject.0 as f64 + 0.5,
        start.1 + 2.0,
        desired_subject.2 as f64 + 0.5,
    ));
    if global == 0 {
        for lane in WALL_LANES {
            let cell = (
                (anchor.0 + ux * WALL_RANGE + px * lane as f64).round() as i64,
                start.1.round() as i64,
                (anchor.1 + uz * WALL_RANGE + pz * lane as f64).round() as i64,
            );
            raise_landmark(ctx, cell, WALL_HEIGHT, MATERIAL).await;
        }
    }
    ctx.record(
        TYPE_LOOK,
        &format!("vis-stereo-occlusion-setup-{index}"),
        "Resolve the blocked centre station and the complementary lateral peek station.",
        json!({"global_index": global, "axis_deg": r2(axis.to_degrees()),
            "subject_range": SUBJECT_RANGE, "wall_range": WALL_RANGE, "peek_offset": PEEK_OFFSET,
            "subject": {"x": r2(subject.0), "y": r2(subject.1), "z": r2(subject.2)},
            "subject_placed": raised_subject.is_some(), "wall_height": WALL_HEIGHT}),
    );
    sleep_ms(global as u64 * 900).await;

    let mut round = 0usize;
    while !ctx.target_met() {
        let peek = (round + global).is_multiple_of(2);
        let station = (
            anchor.0 - ux * CAMERA_RANGE + if peek { px * PEEK_OFFSET } else { 0.0 },
            anchor.1 - uz * CAMERA_RANGE + if peek { pz * PEEK_OFFSET } else { 0.0 },
        );
        travel_to(ctx, station, TRAVEL_HOP, 1.5).await;
        let faced = ctx.look_until_facing(subject, 6.0, 5_000).await;
        let facing = ctx
            .position()
            .map(|p| look_angles(head_of(p), subject).0)
            .unwrap_or(0.0);
        let mut data = geometry(ctx, peer, subject, facing);
        data["round"] = json!(round);
        data["peek_station"] = json!(peek);
        data["complementary_role"] = json!(if peek {
            "clear_around_wall"
        } else {
            "wall_shadow"
        });
        data["faced"] = json!(faced);
        ctx.record(
            TYPE_LOOK,
            &format!("vis-stereo-occlusion-hold-{index}-{round}"),
            "Hold the complementary blocked or clear view of the shared landmark.",
            data,
        );
        shuffle(ctx, facing, ctx.elapsed_ms() + HOLD_MS, global + round).await;
        round += 1;
    }
}

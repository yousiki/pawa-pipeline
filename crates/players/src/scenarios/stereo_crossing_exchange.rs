//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=true scenery=none far_players=false

use super::*;

/// Exchange opposed stereo stations through an asymmetric X-shaped dogleg, then retrace it.
///
/// The revisit problem's trajectory loop is the exchange itself: every two beats each bot returns
/// to its own opening station after visiting the partner's station. The sight line loop is explicit
/// at every arrival, where `look_until_facing` re-acquires the same landmark instead of treating a
/// timed-out turn as success. The crossed paths use opposite elbows, so the pair changes viewpoint
/// without a head-on collision, and natural terrain remains visible throughout. Live baseline,
/// distance, relative bearing and co-visibility are recorded at both ends of each exchange.
///
/// This is not `stereo_crossover_swap`, which walks a straight diameter with a fixed swerve. Here the
/// two elbows are deliberately offset along the baseline, producing a dogleg X and a distinct route
/// history even when the endpoints are identical. It is also not a gaze fan: the bodies exchange
/// camera positions while the shared landmark stays the visual subject.
const MATERIAL: &str = "minecraft:stone_bricks";
const SUBJECT_HEIGHT: i64 = 3;
const HALF_FOV_DEG: f64 = 51.25;
const END_RANGE: f64 = 10.0;
const END_OFFSET: f64 = 3.0;
const ELBOW_OFFSET: f64 = 6.0;
const HOLD_MS: i64 = 7_500;
const PRESS_MS: u64 = 1_350;
const _: () = assert!(2.0 * END_RANGE - 2.0 * END_OFFSET >= MIN_PEER_GAP);

/// A standing column is exterior only when its support is natural and the vertical ray reaches sky.
fn exterior(ctx: &Ctx, feet: BlockPos) -> bool {
    let support = feet.down(1);
    let Some(id) = ctx.block_id_at(support) else {
        return false;
    };
    let id = id.strip_prefix("minecraft:").unwrap_or(&id);
    let artificial = [
        "brick",
        "plank",
        "concrete",
        "terracotta",
        "glass",
        "wool",
        "slab",
        "stairs",
        "fence",
        "wall",
        "quartz",
        "prismarine",
    ]
    .iter()
    .any(|part| id.contains(part));
    !artificial
        && !id.contains("water")
        && !id.contains("lava")
        && !id.contains("leaves")
        && !id.ends_with("log")
        && block_is_safe_support(ctx, support)
        && (feet.y..=319).all(|y| block_is_air(ctx, BlockPos::new(feet.x, y, feet.z)))
}

/// Fixed endpoints and elbows must be actual open ground, not a block face or cave ceiling.
fn open_station(ctx: &Ctx, desired: (f64, f64), hint: i32) -> Option<(f64, f64)> {
    const OFFSETS: [(i32, i32); 13] = [
        (0, 0),
        (1, 0),
        (-1, 0),
        (0, 1),
        (0, -1),
        (1, 1),
        (1, -1),
        (-1, 1),
        (-1, -1),
        (2, 0),
        (-2, 0),
        (0, 2),
        (0, -2),
    ];
    let bx = desired.0.floor() as i32;
    let bz = desired.1.floor() as i32;
    for (dx, dz) in OFFSETS {
        for delta in 0..=18 {
            for y in [hint + delta, hint - delta] {
                let feet = BlockPos::new(bx + dx, y, bz + dz);
                if (feet.y - hint).abs() > 2 {
                    continue;
                }
                let clear =
                    [(0, 0), (1, 0), (-1, 0), (0, 1), (0, -1)]
                        .into_iter()
                        .all(|(sx, sz)| {
                            let cell = BlockPos::new(feet.x + sx, feet.y, feet.z + sz);
                            block_is_air(ctx, cell) && block_is_air(ctx, cell.up(1))
                        });
                if clear && exterior(ctx, feet) {
                    return Some((feet.x as f64 + 0.5, feet.z as f64 + 0.5));
                }
            }
        }
    }
    None
}

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
    let subject_bearing = look_angles(eye, subject).0;
    let mut out = json!({
        "peer_visible": false,
        "subject_distance": r2((subject.0 - own.0).hypot(subject.2 - own.2)),
        "relative_bearing_deg": r2(delta_deg(facing, subject_bearing)),
        "facing_deg": r2(facing),
        "covisibility": false,
    });
    let Some(head) = peer.and_then(|j| ctx.peer_position(j)).map(head_of) else {
        return out;
    };
    let peer_bearing = look_angles(eye, head).0;
    let so = delta_deg(facing, subject_bearing);
    let po = delta_deg(facing, peer_bearing);
    out["peer_visible"] = json!(true);
    out["baseline"] = json!(r2((head.0 - own.0).hypot(head.2 - own.2)));
    out["peer_relative_bearing_deg"] = json!(r2(po));
    out["subject_in_frame"] = json!(so.abs() <= HALF_FOV_DEG);
    out["peer_in_frame"] = json!(po.abs() <= HALF_FOV_DEG);
    out["covisibility"] = json!(so.abs() <= HALF_FOV_DEG && po.abs() <= HALF_FOV_DEG);
    out
}

async fn dwell(ctx: &mut Ctx, facing: f64, deadline: i64, phase: usize) {
    let dirs = if phase.is_multiple_of(2) {
        [WalkDirection::Forward, WalkDirection::Backward]
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
    let home_side = if global.is_multiple_of(2) { 1.0 } else { -1.0 };
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
    let _home = (
        subject.0 + ux * END_RANGE * home_side + px * END_OFFSET * home_side,
        subject.2 + uz * END_RANGE * home_side + pz * END_OFFSET * home_side,
    );
    ctx.record(
        TYPE_LOOK,
        &format!("vis-stereo-cross-setup-{index}"),
        "Resolve the two opposed endpoints and the offset elbows of the exchange.",
        json!({"global_index": global, "axis_deg": r2(axis.to_degrees()), "end_range": END_RANGE,
            "end_offset": END_OFFSET, "elbow_offset": ELBOW_OFFSET,
            "subject": {"x": r2(subject.0), "y": r2(subject.1), "z": r2(subject.2)},
            "subject_placed": raised.is_some()}),
    );
    sleep_ms(global as u64 * 900).await;

    let mut exchange = 0usize;
    while !ctx.target_met() {
        let visiting_peer = exchange % 2 == 1;
        let side = if visiting_peer { -home_side } else { home_side };
        let hint = ctx.position().map(|p| p.1.floor() as i32).unwrap_or(64);
        let desired_station = (
            subject.0 + ux * END_RANGE * side + px * END_OFFSET * side,
            subject.2 + uz * END_RANGE * side + pz * END_OFFSET * side,
        );
        let Some(station) = open_station(ctx, desired_station, hint) else {
            ctx.pause().await;
            continue;
        };
        let desired_elbow = (
            subject.0 + ux * END_RANGE * 0.15 + px * ELBOW_OFFSET * side,
            subject.2 + uz * END_RANGE * 0.15 + pz * ELBOW_OFFSET * side,
        );
        let elbow = open_station(ctx, desired_elbow, hint);
        if visiting_peer {
            // The elbow side changes with the endpoint, so the two bodies trace crossing diagonals
            // rather than occupying one shared midpoint waypoint.
            if let Some(elbow) = elbow {
                travel_to(ctx, elbow, TRAVEL_HOP, 2.5).await;
            }
        }
        travel_to(ctx, station, TRAVEL_HOP, 1.5).await;
        let faced = ctx.look_until_facing(subject, 6.0, 5_000).await;
        let facing = ctx
            .position()
            .map(|p| look_angles(head_of(p), subject).0)
            .unwrap_or(0.0);
        let mut data = geometry(ctx, peer, subject, facing);
        data["exchange"] = json!(exchange);
        data["visiting_peer_endpoint"] = json!(visiting_peer);
        data["returned_home"] = json!(!visiting_peer);
        data["elbow"] = json!(elbow.map(|p| json!({"x": r2(p.0), "z": r2(p.1)})));
        data["faced"] = json!(faced);
        ctx.record(
            TYPE_LOOK,
            &format!("vis-stereo-cross-hold-{index}-{exchange}"),
            "Hold the exchanged stereo endpoint on the shared landmark.",
            data,
        );
        dwell(ctx, facing, ctx.elapsed_ms() + HOLD_MS, global + exchange).await;
        exchange += 1;
    }
}
